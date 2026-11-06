//! The helper's state machine. [`next`] looks at the run's state file and what
//! the helper saw of the world ([`Seen`]) and names the one thing to do
//! ([`Action`]); [`apply`] does the phase bookkeeping after it was done. Both
//! are pure, so a table of cases covers every rule.
//!
//! Every rule reads the world, not what the helper did last: a step that was
//! carried out but not yet recorded (a crash between the two) shows up as a
//! changed world, so repeating the loop never repeats a destructive step.

use super::state::{Phase, StateFile};
use super::{CREATE_START, scaled};

/// What the helper saw of one container.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ctr0 {
    pub id: String,
    pub running: bool,
    /// Docker's status word: `created`, `running`, `restarting`, `exited`, ...
    pub status: String,
    pub restart_count: i64,
}

/// What the helper saw of the world in one pass.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Seen {
    pub now: u64,
    /// The container called `s.container`: the old agent when its id is
    /// `s.old_container_id`, otherwise the new one.
    pub named: Option<Ctr0>,
    /// The container called `<container>-wren-old-<run>`.
    pub renamed: Option<Ctr0>,
    /// The new agent wrote the confirm marker for this run.
    pub confirmed: bool,
    /// The host's boot id differs from the one in the state file.
    pub boot_changed: bool,
    /// The helper's first pass (it was just started, perhaps after a restart).
    pub fresh_start: bool,
}

/// The one thing the helper does next.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Only the bookkeeping: record the new phase.
    Advance(Phase),
    StopOld,
    RenameOld,
    CreateNew,
    /// Start the new container; once it succeeded the phase is `new_started`.
    StartNew,
    /// The host restarted or the helper did: give the new agent a fresh window.
    ExtendDeadline,
    /// Put the old agent back, for this reason.
    RollBack(String),
    CleanUp,
    Finish,
    Wait,
}

/// A container is dead or fell over: it keeps restarting, or it has stopped.
fn fell_over(c: &Ctr0) -> bool {
    c.restart_count > 0 || matches!(c.status.as_str(), "exited" | "dead" | "restarting")
}

/// A host reboot was seen, the confirm window still runs and the new container
/// is down without ever having restarted: it has not come back yet.
fn rebooting_grace(s: &StateFile, c: &Ctr0, now: u64) -> bool {
    s.reboots_seen > 0
        && now <= s.confirm_deadline
        && c.restart_count == 0
        && matches!(c.status.as_str(), "exited" | "created")
}

/// The next step of run `s` given what was seen. Rules are checked top to
/// bottom; the first match wins.
pub fn next(s: &StateFile, seen: &Seen) -> Action {
    use Phase::*;
    let now = seen.now;
    // `named` is the old agent when it has the old id, else the new one.
    let old = seen.named.as_ref().filter(|c| c.id == s.old_container_id);
    let new = seen.named.as_ref().filter(|c| c.id != s.old_container_id);
    let renamed = seen.renamed.is_some();

    if s.phase.terminal() {
        return Action::Finish;
    }
    // A rollback that was started goes on with its first reason.
    if s.phase == RollingBack {
        return Action::RollBack(s.reason.clone());
    }
    if now > s.hard_deadline {
        return Action::RollBack("the self-update took longer than 30 min".into());
    }
    if seen.boot_changed
        && matches!(
            s.phase,
            HelperStarted | OldStopped | OldRenamed | NewCreated
        )
    {
        return Action::RollBack("the host restarted during the update".into());
    }
    if matches!(s.phase, NewStarted | Confirmed) {
        if seen.boot_changed || seen.fresh_start {
            return Action::ExtendDeadline;
        }
        match new {
            None => return Action::RollBack("the new agent's container is gone".into()),
            // After a reboot the new agent may not be back yet (no restart
            // policy): until the extended deadline that is waiting, not failing.
            Some(c) if rebooting_grace(s, c, now) => return Action::Wait,
            Some(c) if fell_over(c) => {
                return Action::RollBack(format!(
                    "the new agent stopped or restarted ({}, restarts {})",
                    c.status, c.restart_count
                ));
            }
            Some(_) => {}
        }
    }
    // Too slow to create and start it: only when it was not started at all
    // (one that was started and fell over is rule 6 once seen as started).
    let late = now > s.updated_at + scaled(CREATE_START)
        && new.is_none_or(|c| !c.running && c.status == "created");
    match s.phase {
        Prepared => Action::Advance(HelperStarted),
        HelperStarted => match old {
            Some(c) if c.running => Action::StopOld,
            Some(_) => Action::Advance(OldStopped),
            None if renamed => Action::Advance(OldStopped),
            // Neither container seen: nothing to stop, and nothing to wait for.
            None => Action::Wait,
        },
        OldStopped => match old {
            Some(_) => Action::RenameOld,
            None if renamed => Action::Advance(OldRenamed),
            None => Action::Wait,
        },
        OldRenamed | NewCreated if late => {
            Action::RollBack("the new agent couldn't be created or started".into())
        }
        OldRenamed => match new {
            None => Action::CreateNew,
            Some(_) => Action::Advance(NewCreated),
        },
        // Anything but `created` means it was started (and perhaps fell over
        // at once): a second `start` would only hide that.
        NewCreated => match new {
            Some(c) if c.running || c.status != "created" => Action::Advance(NewStarted),
            Some(_) => Action::StartNew,
            None => Action::Wait,
        },
        NewStarted if seen.confirmed => Action::Advance(Confirmed),
        NewStarted if now > s.confirm_deadline => Action::RollBack(format!(
            "the new agent didn't reach Wren within {} min",
            s.confirm_minutes
        )),
        Confirmed if now >= s.updated_at + scaled(s.stable_seconds) => Action::CleanUp,
        _ => Action::Wait,
    }
}

/// The bookkeeping after `a` was carried out (the doing is the helper's).
pub fn apply(s: &mut StateFile, a: &Action, seen: &Seen) {
    let window = seen.now + scaled(s.confirm_minutes * 60);
    match a {
        Action::Advance(p) => {
            s.phase = *p;
            s.updated_at = seen.now;
            if *p == Phase::NewStarted {
                s.confirm_deadline = window;
            }
        }
        // A started new agent is the same step as `Advance(NewStarted)`.
        Action::StartNew => apply(s, &Action::Advance(Phase::NewStarted), seen),
        Action::ExtendDeadline => {
            s.confirm_deadline = s.confirm_deadline.max(window);
            // Stability is measured from after the extension, not from before.
            s.updated_at = seen.now;
            s.reboots_seen += u32::from(seen.boot_changed);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(phase: Phase) -> StateFile {
        StateFile {
            format: 1,
            run_id: 57,
            container: "wren-agent".into(),
            old_container_id: "old".into(),
            phase,
            updated_at: 1000,
            confirm_deadline: 1300,
            hard_deadline: 2800,
            confirm_minutes: 5,
            stable_seconds: 60,
            ..StateFile::default()
        }
    }

    fn c(id: &str, running: bool, status: &str, restarts: i64) -> Option<Ctr0> {
        Some(Ctr0 {
            id: id.into(),
            running,
            status: status.into(),
            restart_count: restarts,
        })
    }

    fn at(now: u64) -> Seen {
        Seen {
            now,
            ..Seen::default()
        }
    }

    #[test]
    fn the_helper_table() {
        use Phase::*;
        let old_up = c("old", true, "running", 0);
        let old_down = c("old", false, "exited", 0);
        let new_up = c("new", true, "running", 0);
        let rows: Vec<(Phase, Seen, Action)> = vec![
            (Done, Seen::default(), Action::Finish),
            (
                Prepared,
                Seen {
                    named: old_up.clone(),
                    ..at(1001)
                },
                Action::Advance(HelperStarted),
            ),
            (
                HelperStarted,
                Seen {
                    named: old_up.clone(),
                    ..at(1001)
                },
                Action::StopOld,
            ),
            (
                HelperStarted,
                Seen {
                    named: old_down.clone(),
                    ..at(1001)
                },
                Action::Advance(OldStopped),
            ),
            (
                OldStopped,
                Seen {
                    named: old_down.clone(),
                    ..at(1001)
                },
                Action::RenameOld,
            ),
            (
                OldStopped,
                Seen {
                    renamed: old_down.clone(),
                    ..at(1001)
                },
                Action::Advance(OldRenamed),
            ),
            (
                OldRenamed,
                Seen {
                    renamed: old_down.clone(),
                    ..at(1001)
                },
                Action::CreateNew,
            ),
            (
                NewCreated,
                Seen {
                    named: c("new", false, "created", 0),
                    ..at(1001)
                },
                Action::StartNew,
            ),
            (
                NewCreated,
                Seen {
                    named: new_up.clone(),
                    ..at(1001)
                },
                Action::Advance(NewStarted),
            ),
            // A new agent that fell over at once is rarely seen running: any
            // status but `created` means it was started.
            (
                NewCreated,
                Seen {
                    named: c("new", false, "restarting", 1),
                    ..at(1001)
                },
                Action::Advance(NewStarted),
            ),
            (
                NewCreated,
                Seen {
                    named: c("new", false, "exited", 2),
                    ..at(1001)
                },
                Action::Advance(NewStarted),
            ),
            (
                NewStarted,
                Seen {
                    named: c("new", false, "restarting", 1),
                    ..at(1100)
                },
                Action::RollBack(
                    "the new agent stopped or restarted (restarting, restarts 1)".into(),
                ),
            ),
            (
                NewStarted,
                Seen {
                    named: c("new", false, "exited", 3),
                    ..at(1100)
                },
                Action::RollBack("the new agent stopped or restarted (exited, restarts 3)".into()),
            ),
            (
                NewStarted,
                Seen {
                    named: new_up.clone(),
                    confirmed: true,
                    ..at(1100)
                },
                Action::Advance(Confirmed),
            ),
            (
                NewStarted,
                Seen {
                    named: new_up.clone(),
                    ..at(1301)
                },
                Action::RollBack("the new agent didn't reach Wren within 5 min".into()),
            ),
            (
                NewStarted,
                Seen {
                    named: c("new", true, "running", 1),
                    ..at(1100)
                },
                Action::RollBack("the new agent stopped or restarted (running, restarts 1)".into()),
            ),
            (
                Confirmed,
                Seen {
                    named: c("new", false, "exited", 0),
                    ..at(1030)
                },
                Action::RollBack("the new agent stopped or restarted (exited, restarts 0)".into()),
            ),
            (
                NewStarted,
                at(1100),
                Action::RollBack("the new agent's container is gone".into()),
            ),
            (
                Confirmed,
                Seen {
                    named: new_up.clone(),
                    ..at(1060)
                },
                Action::CleanUp,
            ),
            (
                Confirmed,
                Seen {
                    named: new_up.clone(),
                    ..at(1059)
                },
                Action::Wait,
            ),
            (
                NewStarted,
                Seen {
                    named: new_up.clone(),
                    fresh_start: true,
                    ..at(1100)
                },
                Action::ExtendDeadline,
            ),
            (
                NewStarted,
                Seen {
                    named: new_up.clone(),
                    boot_changed: true,
                    ..at(1100)
                },
                Action::ExtendDeadline,
            ),
            (
                OldRenamed,
                Seen {
                    boot_changed: true,
                    ..at(1001)
                },
                Action::RollBack("the host restarted during the update".into()),
            ),
            (
                OldStopped,
                at(2801),
                Action::RollBack("the self-update took longer than 30 min".into()),
            ),
            (
                NewCreated,
                Seen {
                    named: c("new", false, "created", 0),
                    ..at(1121)
                },
                Action::RollBack("the new agent couldn't be created or started".into()),
            ),
            // Rule 9, second branch: the old agent is already renamed away.
            (
                HelperStarted,
                Seen {
                    renamed: old_down.clone(),
                    ..at(1001)
                },
                Action::Advance(OldStopped),
            ),
            // Rule 14: the new container exists.
            (
                OldRenamed,
                Seen {
                    named: c("new", false, "created", 0),
                    renamed: old_down.clone(),
                    ..at(1001)
                },
                Action::Advance(NewCreated),
            ),
            // Rule 12 in OldRenamed: no new container in time.
            (
                OldRenamed,
                Seen {
                    renamed: old_down.clone(),
                    ..at(1121)
                },
                Action::RollBack("the new agent couldn't be created or started".into()),
            ),
            // ... but a new agent that was started and fell over is not "late".
            (
                NewCreated,
                Seen {
                    named: c("new", false, "exited", 2),
                    ..at(1121)
                },
                Action::Advance(NewStarted),
            ),
            // Nothing to act on yet: wait.
            (HelperStarted, at(1001), Action::Wait),
            (OldStopped, at(1001), Action::Wait),
            (NewCreated, at(1001), Action::Wait),
            (RollingBack, at(1001), Action::RollBack(String::new())),
        ];
        for (phase, seen, want) in rows {
            assert_eq!(next(&st(phase), &seen), want, "{phase:?} {seen:?}");
        }
    }

    #[test]
    fn a_rollback_resumes_with_its_reason() {
        let mut s = st(Phase::RollingBack);
        s.reason = "the new agent's container is gone".into();
        // Past the hard deadline and after a reboot too: the first reason stays.
        let seen = Seen {
            boot_changed: true,
            ..at(9999)
        };
        assert_eq!(
            next(&s, &seen),
            Action::RollBack("the new agent's container is gone".into())
        );
    }

    #[test]
    fn repeating_an_event_gives_no_second_destructive_action() {
        // After RenameOld succeeded the world shows `renamed`: the next step is
        // bookkeeping, never a second rename or stop.
        let mut s = st(Phase::OldStopped);
        let renamed_world = Seen {
            renamed: c("old", false, "exited", 0),
            ..at(1001)
        };
        let a = next(&s, &renamed_world);
        assert_eq!(a, Action::Advance(Phase::OldRenamed));
        apply(&mut s, &a, &renamed_world);
        assert_eq!(next(&s, &renamed_world), Action::CreateNew);
        let mut s = st(Phase::NewStarted);
        let started = Seen {
            named: c("new", true, "running", 0),
            fresh_start: true,
            ..at(1100)
        };
        apply(&mut s, &Action::ExtendDeadline, &started);
        assert_eq!(s.confirm_deadline, 1400);
    }

    #[test]
    fn a_started_new_agent_opens_the_confirm_window() {
        let seen = at(1100);
        for a in [Action::StartNew, Action::Advance(Phase::NewStarted)] {
            let mut s = st(Phase::NewCreated);
            apply(&mut s, &a, &seen);
            assert_eq!(
                (s.phase, s.updated_at, s.confirm_deadline),
                (Phase::NewStarted, 1100, 1100 + 300),
                "{a:?}"
            );
        }
    }

    #[test]
    fn an_extension_never_shortens_and_counts_the_reboot() {
        let mut s = st(Phase::NewStarted);
        s.confirm_deadline = 5000;
        let boot = Seen {
            boot_changed: true,
            ..at(1100)
        };
        apply(&mut s, &Action::ExtendDeadline, &boot);
        assert_eq!((s.confirm_deadline, s.reboots_seen), (5000, 1));
        let fresh = Seen {
            fresh_start: true,
            ..at(1100)
        };
        apply(&mut s, &Action::ExtendDeadline, &fresh);
        assert_eq!(s.reboots_seen, 1);
    }

    #[test]
    fn a_plain_advance_only_moves_the_phase() {
        let mut s = st(Phase::Prepared);
        let seen = at(1234);
        apply(&mut s, &Action::Advance(Phase::HelperStarted), &seen);
        assert_eq!(
            (s.phase, s.updated_at, s.confirm_deadline),
            (Phase::HelperStarted, 1234, 1300)
        );
        apply(&mut s, &Action::Wait, &seen);
        assert_eq!(s.updated_at, 1234);
    }

    #[test]
    fn after_a_reboot_a_new_agent_without_restart_policy_gets_until_the_extended_deadline() {
        // The host restarted; the helper is back first and the new container
        // is still down (no restart policy): not back yet, not a failure.
        for phase in [Phase::NewStarted, Phase::Confirmed] {
            let mut s = st(phase);
            s.reboots_seen = 1;
            s.confirm_deadline = 1400;
            for down in [c("new", false, "exited", 0), c("new", false, "created", 0)] {
                let within = Seen {
                    named: down.clone(),
                    ..at(1400)
                };
                assert_eq!(next(&s, &within), Action::Wait, "{phase:?} {down:?}");
            }
            // Past the extended deadline rule 6 applies as before.
            let past = Seen {
                named: c("new", false, "exited", 0),
                ..at(1401)
            };
            assert_eq!(
                next(&s, &past),
                Action::RollBack("the new agent stopped or restarted (exited, restarts 0)".into()),
                "{phase:?}"
            );
            // A new agent that restarted, or a gone container, is no grace case.
            let flapping = Seen {
                named: c("new", false, "exited", 1),
                ..at(1100)
            };
            assert!(
                matches!(next(&s, &flapping), Action::RollBack(_)),
                "{phase:?}"
            );
            assert_eq!(
                next(&s, &at(1100)),
                Action::RollBack("the new agent's container is gone".into()),
                "{phase:?}"
            );
            // Without a reboot the grace does not exist.
            s.reboots_seen = 0;
            let down = Seen {
                named: c("new", false, "exited", 0),
                ..at(1100)
            };
            assert!(matches!(next(&s, &down), Action::RollBack(_)), "{phase:?}");
        }
    }

    #[test]
    fn an_extension_restarts_the_stability_window() {
        // Confirmed, stable long enough before the reboot: after the reboot the
        // stability is measured from the extension, not from before.
        let mut s = st(Phase::Confirmed);
        s.hard_deadline = 9000;
        let reboot = Seen {
            named: c("new", true, "running", 0),
            boot_changed: true,
            ..at(5000)
        };
        assert_eq!(next(&s, &reboot), Action::ExtendDeadline);
        apply(&mut s, &Action::ExtendDeadline, &reboot);
        assert_eq!(s.updated_at, 5000);
        let up = |now| Seen {
            named: c("new", true, "running", 0),
            ..at(now)
        };
        assert_eq!(next(&s, &up(5059)), Action::Wait);
        assert_eq!(next(&s, &up(5060)), Action::CleanUp);
    }
}
