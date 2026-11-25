//! When a seat is empty: a dir that looks like a harness's (one of its marker files) but was
//! never used here — no login, no account, nothing running, no transcript ever read, and not
//! listed in `config.toml`. A harness whose seats are all empty and that has no saved login
//! is left out of the dashboard's LIMITS · SEATS and of `swarmy accounts` (an empty seat is
//! where a saved login could go, so with one the section stays).

/// What is known of one seat's use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeatUse {
    /// The seat holds a login (its credentials file names one).
    pub login: bool,
    /// An account is known for the seat (its profile, or its token's claims).
    pub account: bool,
    /// Sessions of the seat running now.
    pub live: usize,
    /// The index has read a transcript of the seat (now or before: its sessions outlive the
    /// harness's own retention).
    pub transcripts: bool,
    /// Listed in `[[seat]]` of `config.toml`: asked for, so always shown.
    pub configured: bool,
}

impl SeatUse {
    /// Whether nothing of the seat is in use: any one sign of use keeps it shown.
    pub fn is_empty(&self) -> bool {
        !self.login && !self.account && self.live == 0 && !self.transcripts && !self.configured
    }
}

/// Whether a harness's section is left out: it has seats, all of them empty, and no saved login
/// of that harness exists.
pub fn hides_harness(seats_empty: &[bool], saved_logins: usize) -> bool {
    !seats_empty.is_empty() && saved_logins == 0 && seats_empty.iter().all(|e| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_is_empty_only_when_nothing_of_it_is_in_use() {
        let empty = SeatUse::default();
        assert!(empty.is_empty());
        // Each sign of use alone keeps the seat shown.
        let used = [
            (
                "login",
                SeatUse {
                    login: true,
                    ..empty
                },
            ),
            (
                "account",
                SeatUse {
                    account: true,
                    ..empty
                },
            ),
            ("live", SeatUse { live: 1, ..empty }),
            (
                "transcripts",
                SeatUse {
                    transcripts: true,
                    ..empty
                },
            ),
            (
                "configured",
                SeatUse {
                    configured: true,
                    ..empty
                },
            ),
        ];
        for (what, u) in used {
            assert!(!u.is_empty(), "{what} alone makes a seat in use");
        }
    }

    #[test]
    fn a_harness_is_hidden_only_when_all_seats_are_empty_and_no_login_is_saved() {
        assert!(hides_harness(&[true], 0));
        assert!(hides_harness(&[true, true], 0));
        assert!(!hides_harness(&[true, false], 0), "one seat in use");
        assert!(!hides_harness(&[true], 1), "a saved login could go in");
        assert!(!hides_harness(&[], 0), "no seat: nothing to hide");
    }
}
