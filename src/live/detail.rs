//! What one agent's expanded row shows beyond the snapshot: its latest tool calls and its life
//! (one point per request). A front end asks for it per agent, only for the agents a person
//! opened, and gets it from [`Reads`]: the two reads run again only when `Index::agent_seq`
//! moved (the index read that agent's transcript further, or another process committed) and on
//! a heavy tick (`build`'s `prev` is `None`), which also picks up a reprice. In between, a call
//! is a map lookup.

use std::sync::Arc;

use super::{Agent, LifePoint, Reads, ToolLine, key_of, life, tool_timeline};
use crate::index::Index;

/// Tool calls kept per agent. The dashboard's list scrolls through more; a browser row shows a
/// screenful.
pub const DETAIL_TOOLS: usize = 40;

#[derive(Debug, Clone, Default)]
pub struct Detail {
    /// Newest first.
    pub tools: Vec<ToolLine>,
    /// Oldest first: the agent's last 2000 requests.
    pub life: Vec<LifePoint>,
}

impl Reads {
    /// The tool calls and the life of `a`, read again only when the agent has something new.
    pub fn detail(&mut self, idx: &Index, a: &Agent) -> anyhow::Result<Arc<Detail>> {
        let seq = idx.agent_seq(key_of(a));
        let k = (a.seat.clone(), a.session_id.clone(), a.agent_id.clone());
        if let Some((read_at, kept)) = self.details.get(&k)
            && *read_at == seq
        {
            return Ok(Arc::clone(kept));
        }
        let fresh = Arc::new(Detail {
            tools: tool_timeline(idx, a, DETAIL_TOOLS)?,
            life: life(idx, a)?,
        });
        self.details.insert(k, (seq, Arc::clone(&fresh)));
        Ok(fresh)
    }

    /// Called by `build` after it settled the agents of a snapshot: a heavy tick forgets every
    /// detail (a reprice or a re-attribution changed rows without a write), a light one forgets
    /// the details of agents that left.
    pub(super) fn settle_details(&mut self, heavy: bool) {
        if heavy {
            self.details.clear();
        } else {
            let agents = &self.agents;
            self.details.retain(|k, _| agents.contains_key(k));
        }
    }
}

/// Where the context fell to under half of the previous request's size after being above
/// 50,000 tokens: Claude Code summarised the conversation. Inferred from the numbers, not a
/// recorded event. Gives the index in `life` of the request after each fall.
pub fn compactions(points: &[LifePoint]) -> Vec<usize> {
    points
        .windows(2)
        .enumerate()
        .filter(|(_, w)| w[1].context * 2 < w[0].context && w[0].context > 50_000)
        .map(|(i, _)| i + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, Utc};

    use super::*;
    use crate::core::ids::SessionId;
    use crate::core::pricing::PriceBook;
    use crate::index::scenarios::{World, assistant, owner};
    use crate::live::{Options, build};

    fn point(secs: i64, context: u64) -> LifePoint {
        LifePoint {
            ts: DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap(),
            cost: 0.01,
            context,
        }
    }

    /// 180k → 40k is a fall to under half after being above 50k: one compaction, at the
    /// request after it. A fall from a small context, a drop to just over half and growth are
    /// not.
    #[test]
    fn a_compaction_is_a_fall_to_under_half_of_a_big_context() {
        let life = [
            point(0, 180_000),
            point(1, 40_000),  // 40k * 2 < 180k: a compaction
            point(2, 45_000),  // growth
            point(3, 30_000),  // 60k < 45k is false: not under half
            point(4, 20_000),  // 40k < 30k is false
            point(5, 9_000),   // 18k < 20k, but 20k is not above 50k
            point(6, 150_000), // growth
            point(7, 60_000),  // 120k < 150k: a compaction
        ];
        assert_eq!(compactions(&life), vec![1, 7]);
        assert!(compactions(&[]).is_empty());
        assert!(compactions(&[point(0, 180_000)]).is_empty());
    }

    fn at(base: DateTime<Utc>, secs: i64, id: &str) -> String {
        let t = (base + Duration::seconds(secs)).to_rfc3339();
        assistant(id, &t, "claude-sonnet-5-5", "/w/shop", (20_000, 2_000), &[])
    }

    /// The detail of an agent is read once per write of that agent: a second call with nothing
    /// new gives the same allocation, a new request makes a fresh read that has it, a heavy
    /// tick forgets it, and an agent that left the snapshot is forgotten on the next light one.
    #[test]
    fn the_detail_is_read_again_only_when_the_agent_has_something_new() {
        let w = World::new(&["work"]);
        let now = Utc::now();
        let base = now - Duration::minutes(5);
        w.append(
            "work",
            "s1",
            &[owner("acc-a"), at(base, 0, "m1"), at(base, 30, "m2")],
        );
        let book = PriceBook::builtin();
        let mut idx = w.index("i.sqlite");
        w.refresh(&mut idx, &[], &book);
        let procs = crate::procs::ProcTable::empty();
        let opts = Options::default();
        let mut reads = Reads::default();
        let heavy = build(&idx, &[], &procs, &opts, now, None, &mut reads).unwrap();
        let agent = heavy.agents[0].clone();
        assert_eq!(agent.session_id, SessionId::new("s1"));

        let first = reads.detail(&idx, &agent).unwrap();
        assert_eq!(first.life.len(), 2, "two requests");
        let again = reads.detail(&idx, &agent).unwrap();
        assert!(Arc::ptr_eq(&first, &again), "nothing new: not read again");

        // A light tick keeps it: the agent is still in the snapshot.
        let light = build(&idx, &[], &procs, &opts, now, Some(&heavy), &mut reads).unwrap();
        assert!(Arc::ptr_eq(&first, &reads.detail(&idx, &agent).unwrap()));

        // A third request: the index wrote the agent, the next call reads it again.
        w.append("work", "s1", &[at(base, 60, "m3")]);
        w.refresh(&mut idx, &[], &book);
        let changed = reads.detail(&idx, &agent).unwrap();
        assert!(!Arc::ptr_eq(&first, &changed));
        assert_eq!(changed.life.len(), 3);

        // A heavy tick forgets everything kept; the next call reads again.
        build(&idx, &[], &procs, &opts, now, None, &mut reads).unwrap();
        let after_heavy = reads.detail(&idx, &agent).unwrap();
        assert!(!Arc::ptr_eq(&changed, &after_heavy));

        // The agent leaves (too old to be in the snapshot): a light tick drops its detail.
        let gone = build(
            &idx,
            &[],
            &procs,
            &opts,
            now + Duration::hours(3),
            Some(&light),
            &mut reads,
        )
        .unwrap();
        assert!(gone.agents.is_empty());
        assert!(reads.details.is_empty());
    }
}
