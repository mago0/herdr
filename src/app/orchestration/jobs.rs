//! Commands the server runs on a trigger, with no pane. Output of a run that exits 0 is news,
//! and news can be delivered to an agent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crate::api::schema::JobAddParams;
use crate::terminal::TerminalId;

pub(crate) const MIN_EVERY_MS: u64 = 1_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 3_600_000;
pub(crate) const MAX_JOBS: usize = 64;
const MAX_NEWS_BYTES: usize = 4 * 1024;
const MAX_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobSpec {
    pub(crate) name: String,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) every: Option<Duration>,
    pub(crate) on_events: Vec<String>,
    pub(crate) on_path: Option<PathBuf>,
    pub(crate) target: Option<TerminalId>,
    pub(crate) owner: Option<TerminalId>,
    pub(crate) timeout: Duration,
    pub(crate) until_ms: Option<u64>,
    pub(crate) dedupe: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Job {
    pub(crate) spec: JobSpec,
    next_run: Option<Instant>,
    pub(crate) running: Option<u64>,
    rerun: Option<Option<TriggerEvent>>,
    pub(crate) runs: u64,
    pub(crate) last_exit: Option<i32>,
    pub(crate) last_run_ms: Option<u64>,
    pub(crate) last_news: Option<String>,
    path_stamp: Option<Option<(SystemTime, u64)>>,
}

/// The event that started a run, by dotted name and as JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TriggerEvent {
    pub(crate) name: String,
    pub(crate) json: String,
}

/// One run to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobRun {
    pub(crate) run_id: u64,
    pub(crate) spec: JobSpec,
    pub(crate) event: Option<TriggerEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobOutcome {
    /// The run exited 0 with output.
    pub(crate) news: bool,
    /// Text to deliver, and where. Absent when there is no news, no target, or the same news again.
    pub(crate) deliver: Option<(TerminalId, String)>,
    /// A trigger fired during the run, so the job runs one more time.
    pub(crate) rerun: Option<Option<TriggerEvent>>,
}

#[derive(Debug, Default)]
pub(crate) struct JobTable {
    jobs: BTreeMap<String, Job>,
    next_run_id: u64,
}

pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.chars().count() <= MAX_NAME_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

/// Checks the parts of a request that do not need the session.
pub(crate) fn validate(
    params: &JobAddParams,
    hookable_events: &[&str],
) -> Result<(), (&'static str, String)> {
    if !valid_name(&params.name) {
        return Err((
            "invalid_job_name",
            "job name must match [a-z0-9][a-z0-9_.-]{0,63}".into(),
        ));
    }
    if params
        .command
        .first()
        .is_none_or(|program| program.is_empty())
    {
        return Err(("invalid_job_command", "command must not be empty".into()));
    }
    if params.every_ms.is_none() && params.on_events.is_empty() && params.on_path.is_none() {
        return Err((
            "invalid_job_trigger",
            "a job needs an interval, an event or a path".into(),
        ));
    }
    if params.every_ms.is_some_and(|every| every < MIN_EVERY_MS) {
        return Err((
            "invalid_job_trigger",
            format!("every_ms must be at least {MIN_EVERY_MS}"),
        ));
    }
    if let Some(unknown) = params
        .on_events
        .iter()
        .find(|event| !hookable_events.contains(&event.as_str()))
    {
        return Err((
            "invalid_job_trigger",
            format!("{unknown} is not an event a job can run on"),
        ));
    }
    if params
        .timeout_ms
        .is_some_and(|timeout| !(MIN_EVERY_MS..=MAX_TIMEOUT_MS).contains(&timeout))
    {
        return Err((
            "invalid_job_timeout",
            format!("timeout_ms must be from {MIN_EVERY_MS} to {MAX_TIMEOUT_MS}"),
        ));
    }
    Ok(())
}

/// The news of a run: its output when it exited 0 and printed something.
pub(crate) fn news_of(exit: Option<i32>, stdout: &str) -> Option<String> {
    let text = stdout.trim();
    if exit != Some(0) || text.is_empty() {
        return None;
    }
    let mut end = text.len().min(MAX_NEWS_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_string())
}

impl JobTable {
    pub(crate) fn len(&self) -> usize {
        self.jobs.len()
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.jobs.contains_key(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Job> {
        self.jobs.get(name)
    }

    /// Adds a job, in place of one with the same name. An interval job is due at once.
    pub(crate) fn add(&mut self, spec: JobSpec, last_news: Option<String>, now: Instant) {
        let next_run = spec.every.map(|_| now);
        self.jobs.insert(
            spec.name.clone(),
            Job {
                spec,
                next_run,
                running: None,
                rerun: None,
                runs: 0,
                last_exit: None,
                last_run_ms: None,
                last_news,
                path_stamp: None,
            },
        );
    }

    pub(crate) fn remove(&mut self, name: &str) -> Option<Job> {
        self.jobs.remove(name)
    }

    /// Starts a run, or notes one more run when the job runs now.
    pub(crate) fn begin(&mut self, name: &str, event: Option<TriggerEvent>) -> Option<JobRun> {
        let job = self.jobs.get_mut(name)?;
        if job.running.is_some() {
            job.rerun = Some(event);
            return None;
        }
        self.next_run_id += 1;
        job.running = Some(self.next_run_id);
        Some(JobRun {
            run_id: self.next_run_id,
            spec: job.spec.clone(),
            event,
        })
    }

    /// Names of interval jobs whose time has come. Each gets its next time set.
    pub(crate) fn take_due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        for job in self.jobs.values_mut() {
            let (Some(every), Some(at)) = (job.spec.every, job.next_run) else {
                continue;
            };
            if now >= at {
                job.next_run = Some(now + every);
                due.push(job.spec.name.clone());
            }
        }
        due
    }

    /// Names of jobs whose watched file differs from the last look. The first look only records.
    pub(crate) fn take_changed_paths(
        &mut self,
        stamp: impl Fn(&std::path::Path) -> Option<(SystemTime, u64)>,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        for job in self.jobs.values_mut() {
            let Some(path) = job.spec.on_path.as_deref() else {
                continue;
            };
            let current = stamp(path);
            if job
                .path_stamp
                .replace(current)
                .is_some_and(|previous| previous != current)
            {
                changed.push(job.spec.name.clone());
            }
        }
        changed
    }

    pub(crate) fn names_for_event(&self, event: &str) -> Vec<String> {
        self.jobs
            .values()
            .filter(|job| job.spec.on_events.iter().any(|on| on == event))
            .map(|job| job.spec.name.clone())
            .collect()
    }

    /// Names of jobs past their end time or tied to a pane for which `gone` is true.
    pub(crate) fn ended(&self, now_ms: u64, gone: impl Fn(&TerminalId) -> bool) -> Vec<String> {
        self.jobs
            .values()
            .filter(|job| {
                job.spec.until_ms.is_some_and(|until| now_ms >= until)
                    || job.spec.target.as_ref().is_some_and(&gone)
                    || job.spec.owner.as_ref().is_some_and(&gone)
            })
            .map(|job| job.spec.name.clone())
            .collect()
    }

    /// Records the end of a run. A result from a run that is not the current one is ignored.
    pub(crate) fn finish(
        &mut self,
        name: &str,
        run_id: u64,
        exit: Option<i32>,
        stdout: &str,
        now_ms: u64,
    ) -> Option<JobOutcome> {
        let job = self.jobs.get_mut(name)?;
        if job.running != Some(run_id) {
            return None;
        }
        job.running = None;
        job.runs += 1;
        job.last_exit = exit;
        job.last_run_ms = Some(now_ms);
        let news = news_of(exit, stdout);
        let repeated = job.spec.dedupe && news.is_some() && news == job.last_news;
        let deliver = match (&news, &job.spec.target) {
            (Some(text), Some(target)) if !repeated => Some((target.clone(), text.clone())),
            _ => None,
        };
        if news.is_some() {
            job.last_news = news.clone();
        }
        Some(JobOutcome {
            news: news.is_some(),
            deliver,
            rerun: job.rerun.take(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> JobSpec {
        JobSpec {
            name: name.into(),
            command: vec!["true".into()],
            cwd: None,
            env: BTreeMap::new(),
            every: Some(Duration::from_secs(60)),
            on_events: Vec::new(),
            on_path: None,
            target: Some(TerminalId::alloc()),
            owner: None,
            timeout: Duration::from_secs(60),
            until_ms: None,
            dedupe: true,
        }
    }

    fn params() -> JobAddParams {
        JobAddParams {
            name: "pr-watch".into(),
            command: vec!["gh".into(), "pr".into(), "checks".into()],
            cwd: None,
            env: Default::default(),
            every_ms: Some(60_000),
            on_events: Vec::new(),
            on_path: None,
            target: None,
            owner_pane_id: None,
            timeout_ms: None,
            until_ms: None,
            dedupe: None,
        }
    }

    const EVENTS: &[&str] = &["pane.closed", "pane.agent_status_changed"];

    fn code(result: Result<(), (&'static str, String)>) -> &'static str {
        result.err().map(|(code, _)| code).unwrap_or("ok")
    }

    #[test]
    fn a_request_needs_a_valid_name_a_command_and_a_trigger() {
        assert_eq!(code(validate(&params(), EVENTS)), "ok");
        for name in ["", "Upper", "-lead", "has space", &"x".repeat(65)] {
            let bad = JobAddParams {
                name: name.into(),
                ..params()
            };
            assert_eq!(code(validate(&bad, EVENTS)), "invalid_job_name", "{name}");
        }
        let no_command = JobAddParams {
            command: Vec::new(),
            ..params()
        };
        assert_eq!(code(validate(&no_command, EVENTS)), "invalid_job_command");
        let no_trigger = JobAddParams {
            every_ms: None,
            ..params()
        };
        assert_eq!(code(validate(&no_trigger, EVENTS)), "invalid_job_trigger");
    }

    #[test]
    fn a_request_with_a_short_interval_an_unknown_event_or_a_bad_time_limit_is_refused() {
        let fast = JobAddParams {
            every_ms: Some(999),
            ..params()
        };
        assert_eq!(code(validate(&fast, EVENTS)), "invalid_job_trigger");
        let unknown = JobAddParams {
            on_events: vec!["job.finished".into()],
            ..params()
        };
        assert_eq!(code(validate(&unknown, EVENTS)), "invalid_job_trigger");
        let known = JobAddParams {
            every_ms: None,
            on_events: vec!["pane.closed".into()],
            ..params()
        };
        assert_eq!(code(validate(&known, EVENTS)), "ok");
        for timeout in [0, 999, MAX_TIMEOUT_MS + 1] {
            let bad = JobAddParams {
                timeout_ms: Some(timeout),
                ..params()
            };
            assert_eq!(code(validate(&bad, EVENTS)), "invalid_job_timeout");
        }
    }

    #[test]
    fn only_a_run_that_exits_zero_with_output_has_news() {
        assert_eq!(
            news_of(Some(0), " PR 12 is red \n").as_deref(),
            Some("PR 12 is red")
        );
        assert_eq!(news_of(Some(0), " \n"), None);
        assert_eq!(news_of(Some(1), "text"), None);
        assert_eq!(news_of(None, "text"), None);
    }

    #[test]
    fn long_news_is_cut_at_a_character_boundary() {
        let news = news_of(Some(0), &"é".repeat(MAX_NEWS_BYTES)).unwrap();
        assert!(news.len() <= MAX_NEWS_BYTES);
        assert!(news.chars().all(|c| c == 'é'));
    }

    #[test]
    fn an_interval_job_is_due_at_once_and_then_once_for_each_interval() {
        let start = Instant::now();
        let mut table = JobTable::default();
        table.add(spec("a"), None, start);
        assert_eq!(table.take_due(start), vec!["a".to_string()]);
        assert!(table.take_due(start + Duration::from_secs(59)).is_empty());
        assert_eq!(
            table.take_due(start + Duration::from_secs(60)),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn a_job_without_an_interval_is_never_due() {
        let mut table = JobTable::default();
        table.add(
            JobSpec {
                every: None,
                on_events: vec!["pane.closed".into()],
                ..spec("a")
            },
            None,
            Instant::now(),
        );
        assert!(table
            .take_due(Instant::now() + Duration::from_secs(9999))
            .is_empty());
        assert_eq!(table.names_for_event("pane.closed"), vec!["a".to_string()]);
        assert!(table.names_for_event("pane.exited").is_empty());
    }

    #[test]
    fn news_goes_to_the_target_and_the_same_news_is_not_sent_again() {
        let mut table = JobTable::default();
        let job = spec("a");
        let target = job.target.clone().unwrap();
        table.add(job, None, Instant::now());

        let run = table.begin("a", None).unwrap();
        let first = table.finish("a", run.run_id, Some(0), "red\n", 1).unwrap();
        assert!(first.news);
        assert_eq!(first.deliver, Some((target.clone(), "red".into())));

        let run = table.begin("a", None).unwrap();
        let again = table.finish("a", run.run_id, Some(0), "red\n", 2).unwrap();
        assert!(again.news);
        assert_eq!(again.deliver, None);

        let run = table.begin("a", None).unwrap();
        let quiet = table.finish("a", run.run_id, Some(1), "", 3).unwrap();
        assert!(!quiet.news);

        let run = table.begin("a", None).unwrap();
        let back = table.finish("a", run.run_id, Some(0), "red\n", 4).unwrap();
        assert_eq!(
            back.deliver, None,
            "no news in between does not reset the last news"
        );

        let run = table.begin("a", None).unwrap();
        let changed = table.finish("a", run.run_id, Some(0), "green", 5).unwrap();
        assert_eq!(changed.deliver, Some((target, "green".into())));
        assert_eq!(table.get("a").unwrap().runs, 5);
    }

    #[test]
    fn without_dedupe_the_same_news_is_sent_each_time() {
        let mut table = JobTable::default();
        table.add(
            JobSpec {
                dedupe: false,
                ..spec("a")
            },
            None,
            Instant::now(),
        );
        for _ in 0..2 {
            let run = table.begin("a", None).unwrap();
            assert!(table
                .finish("a", run.run_id, Some(0), "red", 1)
                .unwrap()
                .deliver
                .is_some());
        }
    }

    #[test]
    fn news_kept_from_before_a_restart_is_not_sent_again() {
        let mut table = JobTable::default();
        table.add(spec("a"), Some("red".into()), Instant::now());
        let run = table.begin("a", None).unwrap();
        assert_eq!(
            table
                .finish("a", run.run_id, Some(0), "red", 1)
                .unwrap()
                .deliver,
            None
        );
    }

    #[test]
    fn a_job_without_a_target_has_news_and_delivers_nothing() {
        let mut table = JobTable::default();
        table.add(
            JobSpec {
                target: None,
                ..spec("a")
            },
            None,
            Instant::now(),
        );
        let run = table.begin("a", None).unwrap();
        let outcome = table.finish("a", run.run_id, Some(0), "red", 1).unwrap();
        assert!(outcome.news);
        assert_eq!(outcome.deliver, None);
    }

    #[test]
    fn a_trigger_during_a_run_makes_one_more_run_with_the_last_event() {
        let mut table = JobTable::default();
        table.add(spec("a"), None, Instant::now());
        let run = table.begin("a", None).unwrap();
        let event = |name: &str| TriggerEvent {
            name: name.into(),
            json: "{}".into(),
        };
        assert_eq!(table.begin("a", Some(event("pane.closed"))), None);
        assert_eq!(table.begin("a", Some(event("pane.exited"))), None);

        let outcome = table.finish("a", run.run_id, Some(1), "", 1).unwrap();
        assert_eq!(outcome.rerun, Some(Some(event("pane.exited"))));
        let next = table.begin("a", None).unwrap();
        assert_eq!(
            table
                .finish("a", next.run_id, Some(1), "", 2)
                .unwrap()
                .rerun,
            None
        );
    }

    #[test]
    fn a_result_from_a_run_that_was_replaced_is_ignored() {
        let mut table = JobTable::default();
        table.add(spec("a"), None, Instant::now());
        let old = table.begin("a", None).unwrap();
        table.add(spec("a"), None, Instant::now());
        assert_eq!(table.finish("a", old.run_id, Some(0), "red", 1), None);
        assert_eq!(table.finish("gone", 1, Some(0), "red", 1), None);
    }

    #[test]
    fn a_job_ends_at_its_end_time_or_when_its_target_or_owner_pane_is_gone() {
        let mut table = JobTable::default();
        let (target, owner) = (TerminalId::alloc(), TerminalId::alloc());
        table.add(
            JobSpec {
                until_ms: Some(100),
                target: None,
                ..spec("timed")
            },
            None,
            Instant::now(),
        );
        table.add(
            JobSpec {
                target: Some(target.clone()),
                ..spec("targeted")
            },
            None,
            Instant::now(),
        );
        table.add(
            JobSpec {
                target: None,
                owner: Some(owner.clone()),
                ..spec("owned")
            },
            None,
            Instant::now(),
        );
        assert!(table.ended(99, |_| false).is_empty());
        assert_eq!(table.ended(100, |_| false), vec!["timed".to_string()]);
        assert_eq!(
            table.ended(0, |t| t == &target),
            vec!["targeted".to_string()]
        );
        assert_eq!(table.ended(0, |t| t == &owner), vec!["owned".to_string()]);
    }

    #[test]
    fn a_watched_file_triggers_on_a_change_and_not_on_the_first_look() {
        let mut table = JobTable::default();
        table.add(
            JobSpec {
                every: None,
                on_path: Some("/tmp/proposals.json".into()),
                ..spec("a")
            },
            None,
            Instant::now(),
        );
        let at =
            |secs: u64, len: u64| Some((SystemTime::UNIX_EPOCH + Duration::from_secs(secs), len));
        assert!(table.take_changed_paths(|_| at(1, 10)).is_empty());
        assert!(table.take_changed_paths(|_| at(1, 10)).is_empty());
        assert_eq!(
            table.take_changed_paths(|_| at(2, 10)),
            vec!["a".to_string()]
        );
        assert_eq!(
            table.take_changed_paths(|_| at(2, 11)),
            vec!["a".to_string()]
        );
        assert_eq!(table.take_changed_paths(|_| None), vec!["a".to_string()]);
        assert!(table.take_changed_paths(|_| None).is_empty());
        assert_eq!(
            table.take_changed_paths(|_| at(3, 1)),
            vec!["a".to_string()]
        );
    }
}
