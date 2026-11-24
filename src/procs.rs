//! Running `claude` processes and their process trees.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

#[derive(Debug, Clone, Serialize)]
pub struct ClaudeProc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cwd: Option<PathBuf>,
    /// `CLAUDE_CONFIG_DIR` from the process environment, if set and readable.
    pub config_dir: Option<PathBuf>,
    pub start_time: u64,
    pub cpu: f32,
    pub memory: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgyProc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cwd: Option<PathBuf>,
    /// The data dir this agy uses: `$HOME/.gemini/antigravity-cli` of its environment (the CLI
    /// keys its dir on `HOME`), when the environment is readable.
    pub app_data_dir: Option<PathBuf>,
    pub start_time: u64,
    pub cpu: f32,
    pub memory: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CodexProc {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cwd: Option<PathBuf>,
    /// The Codex home this process uses: `CODEX_HOME` of its environment, else
    /// `$HOME/.codex`, when the environment is readable.
    pub home: Option<PathBuf>,
    pub start_time: u64,
    pub cpu: f32,
    pub memory: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcNode {
    pub pid: u32,
    pub parent: Option<u32>,
    pub name: String,
    pub cmd: String,
    pub cwd: Option<PathBuf>,
    pub cpu: f32,
    pub memory: u64,
}

/// One snapshot of the process table.
pub struct ProcTable {
    pub claude: Vec<ClaudeProc>,
    pub agy: Vec<AgyProc>,
    pub codex: Vec<CodexProc>,
    pub nodes: HashMap<u32, ProcNode>,
    children: HashMap<u32, Vec<u32>>,
}

fn proc_base(s: &str) -> String {
    let b = s
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(s)
        .to_ascii_lowercase();
    b.trim_end_matches(".exe").to_string()
}

fn is_claude(name: &str, cmd: &[String]) -> bool {
    if proc_base(name) == "claude" {
        return true;
    }
    // `node .../@anthropic-ai/claude-code/cli.js`
    cmd.iter()
        .take(2)
        .any(|a| a.contains("@anthropic-ai/claude-code/") && !a.contains("chrome-native"))
}

fn is_agy(name: &str) -> bool {
    proc_base(name) == "agy"
}

/// Codex subcommands that run no agent (the app server, logins, MCP, completions…). The app
/// server of Codex 0.172 is a `codex app-server` process of its own; the agent is the `codex`
/// that started it.
const CODEX_NON_AGENT: &[&str] = &[
    "app-server",
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "completion",
    "sandbox",
    "debug",
    "apply",
    "cloud",
    "features",
    "help",
];

/// The native `codex` binary running an agent (the npm wrapper is a `node` process whose child
/// is that binary, so it is not counted twice).
fn is_codex(name: &str, cmd: &[String]) -> bool {
    proc_base(name) == "codex"
        && !cmd
            .get(1)
            .is_some_and(|a| CODEX_NON_AGENT.contains(&a.as_str()))
}

impl ProcTable {
    pub fn snapshot() -> ProcTable {
        let Ok(mut sys) = System::new() else {
            return ProcTable::empty();
        };
        let kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet)
            .with_environ(UpdateKind::OnlyIfNotSet)
            .with_memory()
            .with_cpu();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        Self::from_system(&sys)
    }

    /// Takes two samples so CPU usage is meaningful (used by the live view).
    pub fn snapshot_with_cpu(sys: &mut System) -> ProcTable {
        let kind = ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::OnlyIfNotSet)
            .with_environ(UpdateKind::OnlyIfNotSet)
            .with_memory()
            .with_cpu();
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
        Self::from_system(sys)
    }

    /// Only the given processes: enough to tell live sessions apart, at a fraction of the cost
    /// of reading the whole process table.
    pub fn for_pids(sys: &mut System, pids: &[u32]) -> ProcTable {
        let list: Vec<Pid> = pids.iter().map(|p| Pid::from_u32(*p)).collect();
        let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
        sys.refresh_processes_specifics(ProcessesToUpdate::Some(&list), true, kind);
        let mut nodes = HashMap::new();
        for pid in &list {
            if let Some(p) = sys.process(*pid) {
                nodes.insert(
                    pid.as_u32(),
                    ProcNode {
                        pid: pid.as_u32(),
                        parent: p.parent().map(Pid::as_u32),
                        name: p.name().to_string_lossy().into_owned(),
                        cmd: String::new(),
                        cwd: None,
                        cpu: p.cpu_usage(),
                        memory: p.memory(),
                    },
                );
            }
        }
        ProcTable {
            claude: Vec::new(),
            agy: Vec::new(),
            codex: Vec::new(),
            nodes,
            children: HashMap::new(),
        }
    }

    pub fn empty() -> ProcTable {
        ProcTable {
            claude: Vec::new(),
            agy: Vec::new(),
            codex: Vec::new(),
            nodes: HashMap::new(),
            children: HashMap::new(),
        }
    }

    fn from_system(sys: &System) -> ProcTable {
        let mut nodes = HashMap::new();
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut claude = Vec::new();
        let mut agy = Vec::new();
        let mut codex = Vec::new();
        for (pid, p) in sys.processes() {
            // Threads show up as processes on Linux; skip them.
            if p.thread_kind().is_some() {
                continue;
            }
            let pid = pid.as_u32();
            let name = p.name().to_string_lossy().into_owned();
            let cmd: Vec<String> = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            let parent = p.parent().map(Pid::as_u32);
            let cwd = p.cwd().map(PathBuf::from);
            if is_claude(&name, &cmd) {
                let config_dir = p.environ().iter().find_map(|e| {
                    let e = e.to_string_lossy();
                    e.strip_prefix("CLAUDE_CONFIG_DIR=")
                        .filter(|v| !v.is_empty())
                        .map(PathBuf::from)
                });
                claude.push(ClaudeProc {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    config_dir,
                    start_time: p.start_time(),
                    cpu: p.cpu_usage(),
                    memory: p.memory(),
                });
            } else if is_agy(&name) {
                let app_data_dir = p.environ().iter().find_map(|e| {
                    let e = e.to_string_lossy();
                    e.strip_prefix("HOME=")
                        .filter(|v| !v.is_empty())
                        .map(|h| PathBuf::from(h).join(".gemini").join("antigravity-cli"))
                });
                agy.push(AgyProc {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    app_data_dir,
                    start_time: p.start_time(),
                    cpu: p.cpu_usage(),
                    memory: p.memory(),
                });
            } else if is_codex(&name, &cmd) {
                let env = |key: &str| {
                    p.environ().iter().find_map(|e| {
                        let e = e.to_string_lossy();
                        e.strip_prefix(key)
                            .filter(|v| !v.is_empty())
                            .map(PathBuf::from)
                    })
                };
                let home = env("CODEX_HOME=").or_else(|| env("HOME=").map(|h| h.join(".codex")));
                codex.push(CodexProc {
                    pid,
                    parent,
                    cwd: cwd.clone(),
                    home,
                    start_time: p.start_time(),
                    cpu: p.cpu_usage(),
                    memory: p.memory(),
                });
            }
            if let Some(pp) = parent {
                children.entry(pp).or_default().push(pid);
            }
            nodes.insert(
                pid,
                ProcNode {
                    pid,
                    parent,
                    name,
                    cmd: cmd.join(" "),
                    cwd,
                    cpu: p.cpu_usage(),
                    memory: p.memory(),
                },
            );
        }
        claude.sort_by_key(|c| c.start_time);
        agy.sort_by_key(|c| c.start_time);
        codex.sort_by_key(|c| c.start_time);
        ProcTable {
            claude,
            agy,
            codex,
            nodes,
            children,
        }
    }

    pub fn alive(&self, pid: u32) -> bool {
        self.nodes.contains_key(&pid)
    }

    /// All descendants of `pid`, depth-first.
    pub fn descendants(&self, pid: u32) -> Vec<&ProcNode> {
        let mut out = Vec::new();
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            if let Some(kids) = self.children.get(&p) {
                for k in kids {
                    if let Some(n) = self.nodes.get(k) {
                        out.push(n);
                        stack.push(*k);
                    }
                }
            }
        }
        out
    }

    /// CPU % and resident bytes of a process with all its descendants.
    pub fn tree_usage(&self, pid: u32) -> (f32, u64) {
        let own = self
            .nodes
            .get(&pid)
            .map(|n| (n.cpu, n.memory))
            .unwrap_or((0.0, 0));
        self.descendants(pid)
            .iter()
            .fold(own, |(c, m), n| (c + n.cpu, m + n.memory))
    }
}

/// The start time of a process in clock ticks since boot (Linux `/proc/<pid>/stat` field 22), the
/// value Claude Code records as `procStart` to tell a live session from a reused PID.
/// Whether process `pid` runs, looked at once without a process table (the status line, the
/// lock-file readers): `/proc` on Linux, a single-process refresh elsewhere. It used to say yes
/// for every pid off Linux, so a dead session's registry entry showed as "needs you" there
/// (review B-M18).
pub fn pid_alive_once(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        // A machine where the process list cannot be read: taken as running, as before.
        let Ok(mut sys) = System::new() else {
            return true;
        };
        let spid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        sys.process(spid).is_some()
    }
}

pub fn proc_start_ticks(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name may contain spaces and parentheses; fields resume after the last ')'.
        let rest = &stat[stat.rfind(')')? + 2..];
        // `rest` starts at field 3 (state); field 22 is index 19 here.
        rest.split_whitespace().nth(19).map(str::to_string)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_claude() {
        assert!(is_claude("claude", &[]));
        assert!(is_claude("claude.exe", &[]));
        assert!(is_claude(
            "node",
            &[
                "node".into(),
                "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js".into()
            ]
        ));
        assert!(!is_claude(
            "chrome-native-host",
            &["/usr/lib/claude-desktop/resources/chrome-native-host".into()]
        ));
        assert!(!is_claude(
            "zsh",
            &["zsh".into(), "-c".into(), "claude".into()]
        ));
    }

    #[test]
    fn recognises_a_codex_agent_not_its_app_server() {
        assert!(is_codex("codex", &["codex".into()]));
        assert!(is_codex(
            "codex",
            &["codex".into(), "exec".into(), "fix the tests".into()]
        ));
        assert!(!is_codex(
            "codex",
            &["codex".into(), "app-server".into(), "--listen".into()]
        ));
        assert!(!is_codex("codex", &["codex".into(), "login".into()]));
        assert!(!is_codex(
            "node",
            &[
                "node".into(),
                "/usr/lib/node_modules/@openai/codex/bin/codex.js".into()
            ]
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_start_ticks_are_numeric() {
        let t = proc_start_ticks(std::process::id()).unwrap();
        assert!(t.parse::<u64>().is_ok(), "{t}");
    }
}
