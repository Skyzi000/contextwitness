use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;

use cw_core::privacy::is_blacklisted;

use crate::stream::Close;

pub(crate) const GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Target {
    pub pid: u32,
    pub created: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Process {
    pub parent: u32,
    /// The creation sequence number, or with the fallback list the creation time; either tells a
    /// reused pid apart.
    pub created: u64,
    pub exe: String,
}

pub(crate) type Table = HashMap<u32, Process>;

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub keep: BTreeSet<Target>,
    pub discard: BTreeSet<Target>,
}

pub(crate) fn select(
    table: &Table,
    active: &HashSet<u32>,
    open: &[(Target, Duration)],
    own_pid: u32,
    blacklist: &[String],
) -> Selection {
    let tree = Tree::new(table, blacklist);
    let alive = |target: &Target| {
        table
            .get(&target.pid)
            .is_some_and(|process| process.created == target.created)
    };

    let discard = open
        .iter()
        .map(|(target, _)| *target)
        .filter(|target| tree.blocked(target.pid))
        .collect();

    let lingering = open
        .iter()
        .filter(|(target, idle)| *idle <= GRACE && alive(target))
        .map(|(target, _)| target.pid);
    let mut playing: Vec<Target> = active
        .iter()
        .copied()
        .chain(lingering)
        .collect::<BTreeSet<u32>>()
        .into_iter()
        .filter(|pid| *pid != 0 && *pid != own_pid)
        .filter_map(|pid| {
            let process = table.get(&pid)?;
            (!process.exe.eq_ignore_ascii_case("audiodg.exe") && !tree.blocked(pid)).then_some(
                Target {
                    pid,
                    created: process.created,
                },
            )
        })
        .collect();

    playing.sort_by_key(|target| (target.created, target.pid));
    let mut kept_pids = HashSet::new();
    let mut keep = BTreeSet::new();
    for target in playing {
        if !tree
            .parent(target.pid)
            .is_some_and(|parent| kept_pids.contains(&parent))
        {
            kept_pids.insert(target.pid);
            keep.insert(target);
        }
    }

    Selection { keep, discard }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    Paused,
    Unjudged,
    Blacklisted,
    Exited,
    StoppedPlaying,
    ParentRecorded,
}

impl Ending {
    pub(crate) fn close(self) -> Close {
        match self {
            Self::Paused | Self::Unjudged | Self::Blacklisted => Close::Discard,
            Self::Exited | Self::StoppedPlaying | Self::ParentRecorded => Close::Flush,
        }
    }

    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Paused => "capture is paused",
            Self::Unjudged => "the process list could not be read",
            Self::Blacklisted => "a blacklisted process is in its tree",
            Self::Exited => "it exited",
            Self::StoppedPlaying => "it stopped playing",
            Self::ParentRecorded => "its parent is recorded",
        }
    }
}

pub(crate) struct Seen {
    pub target: Target,
    pub idle: Duration,
    pub recording: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub endings: Vec<(Target, Ending)>,
    pub openings: Vec<Target>,
}

pub(crate) fn judge(
    table: &Table,
    sessions: Option<&HashSet<u32>>,
    streams: &[Seen],
    own_pid: u32,
    blacklist: &[String],
) -> Verdict {
    let alive = |target: &Target| {
        table
            .get(&target.pid)
            .is_some_and(|process| process.created == target.created)
    };
    let recording = || streams.iter().filter(|seen| seen.recording);

    let mut active: HashSet<u32> = match sessions {
        Some(active) => active.clone(),
        None => recording()
            .filter(|seen| alive(&seen.target))
            .map(|seen| seen.target.pid)
            .collect(),
    };
    for seen in streams {
        if !seen.recording && alive(&seen.target) {
            active.remove(&seen.target.pid);
        }
    }
    let open: Vec<(Target, Duration)> = recording().map(|seen| (seen.target, seen.idle)).collect();
    let selection = select(table, &active, &open, own_pid, blacklist);

    let endings = streams
        .iter()
        .filter(|seen| !selection.keep.contains(&seen.target))
        .filter_map(|seen| {
            let ending = if !seen.recording {
                (!alive(&seen.target)).then_some(Ending::Exited)?
            } else if selection.discard.contains(&seen.target) {
                Ending::Blacklisted
            } else if !alive(&seen.target) {
                Ending::Exited
            } else if seen.idle > GRACE {
                Ending::StoppedPlaying
            } else {
                Ending::ParentRecorded
            };
            Some((seen.target, ending))
        })
        .collect();
    let openings = selection
        .keep
        .iter()
        .filter(|target| !streams.iter().any(|seen| seen.target == **target))
        .copied()
        .collect();
    Verdict { endings, openings }
}

struct Tree<'a> {
    table: &'a Table,
    blacklist: &'a [String],
    children: HashMap<u32, Vec<u32>>,
}

impl<'a> Tree<'a> {
    fn new(table: &'a Table, blacklist: &'a [String]) -> Self {
        let mut tree = Self {
            table,
            blacklist,
            children: HashMap::new(),
        };
        if !blacklist.is_empty() {
            for pid in table.keys() {
                if let Some(parent) = tree.parent(*pid) {
                    tree.children.entry(parent).or_default().push(*pid);
                }
            }
        }
        tree
    }

    fn parent(&self, pid: u32) -> Option<u32> {
        let process = self.table.get(&pid)?;
        let parent = self.table.get(&process.parent)?;
        (process.parent != 0 && process.parent != pid && parent.created <= process.created)
            .then_some(process.parent)
    }

    fn listed(&self, pid: u32) -> bool {
        self.table.get(&pid).is_some_and(|process| {
            process.exe.is_empty() || is_blacklisted(&process.exe, self.blacklist)
        })
    }

    fn blocked(&self, pid: u32) -> bool {
        if self.blacklist.is_empty() {
            return false;
        }
        let mut seen = HashSet::new();
        let mut lineage = Some(pid);
        while let Some(current) = lineage.filter(|current| seen.insert(*current)) {
            if self.listed(current) {
                return true;
            }
            lineage = self.parent(current);
        }

        let mut seen = HashSet::from([pid]);
        let mut pending = vec![pid];
        while let Some(current) = pending.pop() {
            for child in self.children.get(&current).into_iter().flatten() {
                if seen.insert(*child) {
                    if self.listed(*child) {
                        return true;
                    }
                    pending.push(*child);
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN_PID: u32 = 999;

    fn table(rows: &[(u32, u32, u64, &str)]) -> Table {
        rows.iter()
            .map(|&(pid, parent, created, exe)| {
                (
                    pid,
                    Process {
                        parent,
                        created,
                        exe: exe.to_owned(),
                    },
                )
            })
            .collect()
    }

    fn target(pid: u32, created: u64) -> Target {
        Target { pid, created }
    }

    fn targets(list: &[(u32, u64)]) -> BTreeSet<Target> {
        list.iter()
            .map(|&(pid, created)| target(pid, created))
            .collect()
    }

    fn listed(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn kept(
        table: &Table,
        active: &[u32],
        open: &[(Target, Duration)],
        blacklist: &[&str],
    ) -> BTreeSet<Target> {
        select(
            table,
            &active.iter().copied().collect(),
            open,
            OWN_PID,
            &listed(blacklist),
        )
        .keep
    }

    #[test]
    fn a_playing_process_is_opened_under_its_own_identity() {
        let table = table(&[(10, 1, 100, "talk.exe"), (11, 1, 110, "player.exe")]);
        assert_eq!(kept(&table, &[10], &[], &[]), targets(&[(10, 100)]));
    }

    #[test]
    fn only_the_direct_parent_absorbs_a_playing_child() {
        let table = table(&[
            (10, 1, 100, "launcher.exe"),
            (11, 10, 110, "helper.exe"),
            (12, 11, 120, "talk.exe"),
        ]);
        assert_eq!(
            kept(&table, &[10, 11, 12], &[], &[]),
            targets(&[(10, 100), (12, 120)]),
            "A records B's sound; C sits two levels below A and is recorded by nobody else"
        );
    }

    #[test]
    fn a_child_whose_parent_is_not_playing_is_opened_however_deep_it_sits() {
        let table = table(&[
            (10, 1, 100, "launcher.exe"),
            (11, 10, 110, "helper.exe"),
            (12, 11, 120, "talk.exe"),
        ]);
        assert_eq!(
            kept(&table, &[10, 12], &[], &[]),
            targets(&[(10, 100), (12, 120)])
        );
        assert_eq!(kept(&table, &[12], &[], &[]), targets(&[(12, 120)]));
    }

    #[test]
    fn a_listed_process_or_ancestor_blocks_the_target() {
        let table = table(&[
            (10, 1, 100, "secret.exe"),
            (11, 10, 110, "helper.exe"),
            (12, 11, 120, "talk.exe"),
            (20, 1, 100, "player.exe"),
        ]);
        assert_eq!(
            kept(&table, &[10, 12, 20], &[], &["secret.exe"]),
            targets(&[(20, 100)])
        );
        assert_eq!(
            kept(&table, &[12], &[], &["C:\\Apps\\SECRET.EXE"]),
            targets(&[]),
            "the blacklist is matched the way the screen side matches it"
        );
    }

    #[test]
    fn a_listed_living_descendant_blocks_the_whole_tree_above_it() {
        let table = table(&[
            (10, 1, 100, "talk.exe"),
            (11, 10, 110, "helper.exe"),
            (12, 11, 120, "secret.exe"),
            (20, 1, 100, "player.exe"),
        ]);
        assert_eq!(
            kept(&table, &[10, 11, 20], &[], &["secret.exe"]),
            targets(&[(20, 100)])
        );
    }

    #[test]
    fn an_open_stream_that_became_blocked_is_discarded_even_past_the_grace() {
        let table = table(&[(10, 1, 100, "talk.exe"), (11, 10, 110, "secret.exe")]);
        for idle in [Duration::ZERO, GRACE + Duration::from_secs(1)] {
            let selection = select(
                &table,
                &HashSet::new(),
                &[(target(10, 100), idle)],
                OWN_PID,
                &listed(&["secret.exe"]),
            );
            assert_eq!(selection.keep, targets(&[]));
            assert_eq!(selection.discard, targets(&[(10, 100)]));
        }
    }

    #[test]
    fn an_open_stream_that_merely_ended_is_not_discarded() {
        let table = table(&[(10, 1, 100, "talk.exe"), (11, 10, 110, "player.exe")]);
        let open = [
            (target(10, 100), Duration::ZERO),
            (target(11, 110), Duration::ZERO),
            (target(30, 300), Duration::ZERO),
        ];
        let selection = select(
            &table,
            &[10, 11].into_iter().collect(),
            &open,
            OWN_PID,
            &listed(&["secret.exe"]),
        );
        assert_eq!(selection.keep, targets(&[(10, 100)]));
        assert_eq!(
            selection.discard,
            targets(&[]),
            "merged into its parent (11) and exited (30) both hand their audio on"
        );
    }

    #[test]
    fn a_parent_link_to_a_later_created_process_is_a_reused_pid() {
        let reused_parent_is_listed =
            table(&[(10, 1, 500, "secret.exe"), (11, 10, 110, "talk.exe")]);
        assert_eq!(
            kept(&reused_parent_is_listed, &[11], &[], &["secret.exe"]),
            targets(&[(11, 110)])
        );

        let reused_parent_is_playing =
            table(&[(10, 1, 500, "player.exe"), (11, 10, 110, "talk.exe")]);
        assert_eq!(
            kept(&reused_parent_is_playing, &[10, 11], &[], &[]),
            targets(&[(10, 500), (11, 110)])
        );

        let listed_child_of_a_reused_pid =
            table(&[(10, 1, 500, "talk.exe"), (11, 10, 110, "secret.exe")]);
        assert_eq!(
            kept(&listed_child_of_a_reused_pid, &[10], &[], &["secret.exe"]),
            targets(&[(10, 500)])
        );
    }

    #[test]
    fn an_open_stream_whose_pid_now_belongs_to_another_process_is_not_kept() {
        let table = table(&[(10, 1, 500, "player.exe")]);
        let open = [(target(10, 100), Duration::ZERO)];
        assert_eq!(
            kept(&table, &[], &open, &[]),
            targets(&[]),
            "the old process is gone and the new one is not playing"
        );
        assert_eq!(kept(&table, &[10], &open, &[]), targets(&[(10, 500)]));
    }

    #[test]
    fn an_open_stream_whose_pid_a_listed_process_took_over_is_discarded() {
        let table = table(&[(10, 1, 500, "secret.exe")]);
        let selection = select(
            &table,
            &HashSet::new(),
            &[(target(10, 100), Duration::ZERO)],
            OWN_PID,
            &listed(&["secret.exe"]),
        );
        assert_eq!(selection.keep, targets(&[]));
        assert_eq!(selection.discard, targets(&[(10, 100)]));
    }

    #[test]
    fn pid_zero_missing_processes_this_process_and_audiodg_are_never_opened() {
        let table = table(&[
            (0, 0, 0, ""),
            (OWN_PID, 1, 100, "contextwitness.exe"),
            (40, 1, 100, "AudioDG.exe"),
            (41, 1, 100, "talk.exe"),
        ]);
        assert_eq!(
            kept(&table, &[0, OWN_PID, 40, 41, 77], &[], &[]),
            targets(&[(41, 100)])
        );
    }

    #[test]
    fn a_child_of_this_process_or_of_audiodg_is_opened_on_its_own() {
        let table = table(&[
            (OWN_PID, 1, 100, "contextwitness.exe"),
            (40, 1, 100, "audiodg.exe"),
            (50, OWN_PID, 200, "talk.exe"),
            (51, 40, 200, "player.exe"),
        ]);
        assert_eq!(
            kept(&table, &[OWN_PID, 40, 50, 51], &[], &[]),
            targets(&[(50, 200), (51, 200)])
        );
    }

    #[test]
    fn an_open_stream_inside_the_grace_counts_as_playing() {
        let table = table(&[(10, 1, 100, "launcher.exe"), (11, 10, 110, "talk.exe")]);
        let inside = [(target(10, 100), GRACE)];
        assert_eq!(
            kept(&table, &[11], &inside, &[]),
            targets(&[(10, 100)]),
            "the parent stays open and keeps absorbing its child"
        );
        let past = [(target(10, 100), GRACE + Duration::from_millis(1))];
        assert_eq!(kept(&table, &[11], &past, &[]), targets(&[(11, 110)]));
        assert_eq!(kept(&table, &[], &past, &[]), targets(&[]));
    }

    #[test]
    fn an_unnamed_process_is_treated_as_listed_only_when_a_blacklist_is_set() {
        let table = table(&[(10, 1, 100, ""), (11, 10, 110, "talk.exe")]);
        assert_eq!(kept(&table, &[11], &[], &["secret.exe"]), targets(&[]));
        assert_eq!(kept(&table, &[11], &[], &[]), targets(&[(11, 110)]));
    }

    #[test]
    fn a_parent_cycle_ends_the_walk() {
        let table = table(&[(10, 11, 100, "talk.exe"), (11, 10, 100, "player.exe")]);
        let selection = select(
            &table,
            &[10, 11].into_iter().collect(),
            &[],
            OWN_PID,
            &listed(&["secret.exe"]),
        );
        assert_eq!(
            selection.keep,
            targets(&[(10, 100)]),
            "with equal stamps the lower pid is decided first and takes the other in"
        );
    }

    #[test]
    fn a_parent_with_the_higher_pid_still_absorbs_its_child() {
        let table = table(&[
            (30, 1, 100, "launcher.exe"),
            (20, 30, 200, "helper.exe"),
            (10, 20, 300, "talk.exe"),
        ]);
        assert_eq!(
            kept(&table, &[10, 20, 30], &[], &[]),
            targets(&[(30, 100), (10, 300)])
        );
    }

    fn seen(pid: u32, created: u64, idle: Duration, recording: bool) -> Seen {
        Seen {
            target: target(pid, created),
            idle,
            recording,
        }
    }

    fn judged(table: &Table, sessions: Option<&[u32]>, streams: &[Seen]) -> Verdict {
        let sessions: Option<HashSet<u32>> =
            sessions.map(|active| active.iter().copied().collect());
        judge(
            table,
            sessions.as_ref(),
            streams,
            OWN_PID,
            &listed(&["secret.exe"]),
        )
    }

    const PAST_GRACE: Duration = Duration::from_secs(31);

    #[test]
    fn what_a_stream_still_holds_is_discarded_for_privacy_and_handed_on_otherwise() {
        for ending in [Ending::Paused, Ending::Unjudged, Ending::Blacklisted] {
            assert_eq!(ending.close(), Close::Discard, "{ending:?}");
        }
        for ending in [
            Ending::Exited,
            Ending::StoppedPlaying,
            Ending::ParentRecorded,
        ] {
            assert_eq!(ending.close(), Close::Flush, "{ending:?}");
        }
    }

    #[test]
    fn each_stream_that_is_not_kept_ends_for_its_own_reason() {
        let table = table(&[
            (10, 1, 100, "talk.exe"),
            (11, 10, 110, "secret.exe"),
            (20, 1, 100, "launcher.exe"),
            (21, 20, 110, "player.exe"),
            (30, 1, 100, "player.exe"),
            (40, 1, 100, "talk.exe"),
        ]);
        let streams = [
            seen(10, 100, Duration::ZERO, true),
            seen(20, 100, Duration::ZERO, true),
            seen(21, 110, Duration::ZERO, true),
            seen(30, 100, PAST_GRACE, true),
            seen(40, 100, Duration::ZERO, true),
            seen(50, 100, Duration::ZERO, true),
        ];
        let verdict = judged(&table, Some(&[10, 20, 21, 40, 50]), &streams);
        assert_eq!(
            verdict.endings,
            [
                (target(10, 100), Ending::Blacklisted),
                (target(21, 110), Ending::ParentRecorded),
                (target(30, 100), Ending::StoppedPlaying),
                (target(50, 100), Ending::Exited),
            ]
        );
        assert_eq!(verdict.openings, []);
    }

    #[test]
    fn a_playing_process_without_a_stream_is_opened_once() {
        let table = table(&[(10, 1, 100, "talk.exe"), (20, 1, 100, "player.exe")]);
        let streams = [seen(10, 100, Duration::ZERO, true)];
        let verdict = judged(&table, Some(&[10, 20]), &streams);
        assert_eq!(verdict.endings, []);
        assert_eq!(verdict.openings, [target(20, 100)]);
    }

    #[test]
    fn a_stream_that_is_not_recording_does_not_stand_in_for_its_child() {
        let table = table(&[(10, 1, 100, "launcher.exe"), (11, 10, 110, "talk.exe")]);
        let failed_parent = [seen(10, 100, Duration::ZERO, false)];
        let verdict = judged(&table, Some(&[10, 11]), &failed_parent);
        assert_eq!(
            verdict.openings,
            [target(11, 110)],
            "the child is recorded, and the parent is not opened a second time"
        );
        assert_eq!(
            verdict.endings,
            [],
            "the parent keeps its place until retried"
        );

        let recording_parent = [seen(10, 100, Duration::ZERO, true)];
        assert_eq!(
            judged(&table, Some(&[10, 11]), &recording_parent).openings,
            []
        );
    }

    #[test]
    fn a_stream_that_is_not_recording_is_dropped_once_its_process_is_gone() {
        let table = table(&[(10, 1, 500, "player.exe")]);
        let streams = [seen(10, 100, Duration::ZERO, false)];
        let verdict = judged(&table, Some(&[10]), &streams);
        assert_eq!(verdict.endings, [(target(10, 100), Ending::Exited)]);
        assert_eq!(
            verdict.openings,
            [target(10, 500)],
            "the process that took over the pid is judged on its own"
        );
    }

    #[test]
    fn without_a_session_list_nothing_opens_or_idles_out_but_exits_and_the_blacklist_still_close() {
        let table = table(&[
            (10, 1, 100, "talk.exe"),
            (11, 10, 110, "secret.exe"),
            (20, 1, 100, "player.exe"),
            (30, 1, 100, "talk.exe"),
            (40, 1, 500, "player.exe"),
        ]);
        let streams = [
            seen(10, 100, Duration::ZERO, true),
            seen(20, 100, PAST_GRACE, true),
            seen(40, 100, Duration::ZERO, true),
        ];
        let verdict = judged(&table, None, &streams);
        assert_eq!(
            verdict.endings,
            [
                (target(10, 100), Ending::Blacklisted),
                (target(40, 100), Ending::Exited),
            ]
        );
        assert_eq!(
            verdict.openings,
            [],
            "neither the unopened 30 nor the process that took over pid 40"
        );
    }
}
