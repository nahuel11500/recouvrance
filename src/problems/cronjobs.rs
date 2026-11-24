//! CronJobs that stopped running, for the Problems scan: one that should have run by its
//! schedule but hasn't, and one whose runs keep failing. kubectl's columns have no last
//! success, so they are read from their objects; CronJobs are few and small.
//!
//! Suspended CronJobs are left out: not running is what was asked of them.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use kube::Client;
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::{Problem, Severity};
use crate::cron::Cron;
use crate::k8s::{self, Kind};

/// Seconds a run may start late before it counts as missed: the controller checks every
/// few seconds, and the Job takes a moment to be created.
const GRACE: i64 = 60;

/// Runs scheduled since the last success, the last one included, before a CronJob counts
/// as failing. The last may still be running, so at least two have finished without one.
const FAILED_RUNS: usize = 3;

/// The most runs counted since the last success (a schedule every minute adds up fast).
const COUNT_MAX: usize = 100;

/// What the scan reads of a CronJob: its schedule, whether it is suspended, and when it
/// last ran and last succeeded. The Job template and managedFields are skipped as the
/// list is parsed.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct LeanCronJob {
    metadata: LeanMeta,
    spec: LeanSpec,
    status: LeanStatus,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanMeta {
    name: Option<String>,
    namespace: Option<String>,
    creation_timestamp: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanSpec {
    schedule: Option<String>,
    time_zone: Option<String>,
    suspend: Option<bool>,
    starting_deadline_seconds: Option<i64>,
    concurrency_policy: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanStatus {
    last_schedule_time: Option<String>,
    last_successful_time: Option<String>,
    /// Only how many.
    #[serde(deserialize_with = "k8s::null_default")]
    active: Vec<IgnoredAny>,
}

fn time(t: Option<&str>) -> Option<Timestamp> {
    t.and_then(|t| t.parse().ok())
}

/// The schedule's time zone and its five fields: `spec.timeZone`, else a `CRON_TZ=` or
/// `TZ=` prefix of the schedule (the older way), else none.
fn zone_and_fields<'a>(schedule: &'a str, time_zone: Option<&'a str>) -> (Option<&'a str>, &'a str) {
    let schedule = schedule.trim();
    let prefixed = ["CRON_TZ=", "TZ="].iter().find_map(|p| {
        let (zone, rest) = schedule.strip_prefix(p)?.split_once(char::is_whitespace)?;
        Some((zone, rest.trim()))
    });
    let (zone, fields) = prefixed.map_or((None, schedule), |(z, f)| (Some(z), f));
    (time_zone.map(str::trim).filter(|z| !z.is_empty()).or(zone), fields)
}

/// Judge a CronJob at `now`; None when it runs as its schedule says (or the schedule
/// doesn't read here: nothing is claimed then).
///
/// Stopped: the second time it should have fired since it last did (since it was created
/// when it never did), plus `startingDeadlineSeconds`, has passed. Two intervals, so a run
/// a little late, or a controller in another time zone than UTC for a schedule without
/// one, isn't called missed. Failing: `FAILED_RUNS` runs or more were scheduled since its
/// last success (since it was created when it never had one).
pub fn judge(job: &LeanCronJob, now: Timestamp) -> Option<(Severity, String)> {
    if job.spec.suspend == Some(true) {
        return None;
    }
    let (zone, fields) = zone_and_fields(job.spec.schedule.as_deref()?, job.spec.time_zone.as_deref());
    let cron = Cron::parse(fields).ok()?;
    let created = time(job.metadata.creation_timestamp.as_deref());
    let last_run = time(job.status.last_schedule_time.as_deref());
    let tz = match zone {
        None => TimeZone::UTC,
        Some(name) => {
            if let Ok(tz) = TimeZone::get(name) {
                tz
            } else {
                // The controller can't place its runs either, and never starts one. This
                // machine's time zone list may differ from the cluster's: only say so when
                // it never ran.
                let old = created.is_some_and(|c| now.as_second() - c.as_second() > 3600);
                return (last_run.is_none() && old)
                    .then(|| (Severity::Warning, format!("never ran: its time zone `{name}` is unknown")));
            }
        }
    };
    let schedule = match zone {
        Some(z) => format!("`{fields}` ({z})"),
        None => format!("`{fields}`"),
    };

    let from = last_run.or(created)?;
    let first = cron.next_after(&from.to_zoned_datetime(tz.clone()))?;
    let second = cron.next_after(&first)?;
    let deadline = second
        .timestamp()
        .as_second()
        .saturating_add(job.spec.starting_deadline_seconds.unwrap_or(0).max(0))
        .saturating_add(GRACE);
    if now.as_second() > deadline {
        let mut reason = match last_run {
            Some(t) => {
                format!("no run for {}, schedule {schedule}", k8s::duration_text(now.as_second() - t.as_second()))
            }
            None => format!("never ran, schedule {schedule}"),
        };
        // Under Forbid a Job that never ends holds every run after it.
        if job.spec.concurrency_policy.as_deref() == Some("Forbid") && !job.status.active.is_empty() {
            reason.push_str(": a Job still running holds the next runs (concurrencyPolicy Forbid)");
        }
        return Some((Severity::Warning, reason));
    }

    let last_run = last_run?;
    let last_success = time(job.status.last_successful_time.as_deref());
    let since = last_success.or(created)?;
    let mut runs = 0;
    let mut t = since.to_zoned_datetime(tz);
    while runs < COUNT_MAX {
        match cron.next_after(&t) {
            Some(next) if next.timestamp() <= last_run => {
                runs += 1;
                t = next;
            }
            _ => break,
        }
    }
    if runs < FAILED_RUNS {
        return None;
    }
    let count = if runs == COUNT_MAX { format!("{COUNT_MAX}+") } else { runs.to_string() };
    let reason = match last_success {
        Some(s) => format!(
            "last success {} ago, {count} runs since, schedule {schedule}",
            k8s::duration_text(now.as_second() - s.as_second())
        ),
        None => format!("no run succeeded in {count}, schedule {schedule}"),
    };
    Some((Severity::Warning, reason))
}

/// The CronJobs of `namespace` (every one when None) that stopped running or keep
/// failing: one list from the API server's cache, parsed as it streams in.
pub async fn problems(client: Client, kind: &Kind, namespace: Option<String>) -> anyhow::Result<Vec<Problem>> {
    let items = k8s::lists::list_lean_cached::<LeanCronJob>(client, kind.ar.clone(), namespace).await?;
    let now = Timestamp::now();
    Ok(items
        .iter()
        .filter_map(|job| {
            let (severity, reason) = judge(job, now)?;
            Some(Problem {
                severity,
                kind: kind.kubectl_name(),
                label: kind.ar.kind.clone(),
                namespace: job.metadata.namespace.clone(),
                name: job.metadata.name.clone()?,
                reason,
                age: time(job.metadata.creation_timestamp.as_deref()),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(t: &str) -> Timestamp {
        t.parse().unwrap()
    }

    fn cronjob(schedule: &str, created: &str, last_run: Option<&str>, last_success: Option<&str>) -> LeanCronJob {
        serde_json::from_value(serde_json::json!({
            "metadata": {"name": "backup", "namespace": "shop", "creationTimestamp": created},
            "spec": {"schedule": schedule, "jobTemplate": {"spec": {}}},
            "status": {"lastScheduleTime": last_run, "lastSuccessfulTime": last_success, "active": null},
        }))
        .unwrap()
    }

    fn reason(job: &LeanCronJob, now: &str) -> Option<String> {
        judge(job, at(now)).map(|(s, r)| {
            assert_eq!(s, Severity::Warning);
            r
        })
    }

    #[test]
    fn a_cronjob_that_runs_on_time_is_fine() {
        // Daily at 02:00, ran and succeeded last night.
        let job =
            cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T02:00:00Z"), Some("2026-11-24T02:05:00Z"));
        assert_eq!(reason(&job, "2026-11-24T10:00:00Z"), None);
        // One run missed is late, not stopped.
        assert_eq!(reason(&job, "2026-11-25T23:00:00Z"), None);
        // Created a moment ago, never ran yet.
        let new = cronjob("0 2 * * *", "2026-11-24T09:00:00Z", None, None);
        assert_eq!(reason(&new, "2026-11-25T10:00:00Z"), None);
    }

    #[test]
    fn a_cronjob_that_stopped_running_shows() {
        let job =
            cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-11-21T02:00:00Z"), Some("2026-11-21T02:05:00Z"));
        // Due on the 22nd and 23rd: stopped once the 23rd's run is a minute late.
        assert_eq!(reason(&job, "2026-11-23T02:00:30Z"), None);
        assert_eq!(reason(&job, "2026-11-23T02:01:01Z").as_deref(), Some("no run for 2d, schedule `0 2 * * *`"));
        let never = cronjob("*/10 * * * *", "2026-11-24T08:00:00Z", None, None);
        assert_eq!(reason(&never, "2026-11-24T08:19:00Z"), None);
        assert_eq!(reason(&never, "2026-11-24T08:21:01Z").as_deref(), Some("never ran, schedule `*/10 * * * *`"));
    }

    #[test]
    fn a_suspended_cronjob_is_left_out() {
        let mut job = cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-10-01T02:00:00Z"), None);
        assert!(reason(&job, "2026-11-24T10:00:00Z").is_some());
        job.spec.suspend = Some(true);
        assert_eq!(reason(&job, "2026-11-24T10:00:00Z"), None);
    }

    #[test]
    fn the_starting_deadline_gives_it_longer() {
        let mut job =
            cronjob("0 * * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T08:00:00Z"), Some("2026-11-24T08:05:00Z"));
        assert!(reason(&job, "2026-11-24T10:02:00Z").is_some());
        job.spec.starting_deadline_seconds = Some(600);
        assert_eq!(reason(&job, "2026-11-24T10:02:00Z"), None);
        assert!(reason(&job, "2026-11-24T10:12:00Z").is_some());
    }

    #[test]
    fn a_job_held_under_forbid_says_so() {
        let mut job = cronjob("0 * * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T06:00:00Z"), None);
        job.spec.concurrency_policy = Some("Forbid".into());
        job.status.active = vec![IgnoredAny];
        let r = reason(&job, "2026-11-24T10:00:00Z").unwrap();
        assert!(r.starts_with("no run for 4h"), "{r}");
        assert!(r.ends_with("holds the next runs (concurrencyPolicy Forbid)"), "{r}");
    }

    #[test]
    fn runs_that_keep_failing_show() {
        // Runs every night on time, the last success four nights ago.
        let job =
            cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T02:00:00Z"), Some("2026-11-20T02:10:00Z"));
        assert_eq!(
            reason(&job, "2026-11-24T10:00:00Z").as_deref(),
            Some("last success 4d ago, 4 runs since, schedule `0 2 * * *`")
        );
        // Two runs since: the last may still be running.
        let job =
            cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T02:00:00Z"), Some("2026-11-22T02:10:00Z"));
        assert_eq!(reason(&job, "2026-11-24T10:00:00Z"), None);
        // Never succeeded since it was created.
        let job = cronjob("0 * * * *", "2026-11-24T00:30:00Z", Some("2026-11-24T09:00:00Z"), None);
        assert_eq!(
            reason(&job, "2026-11-24T09:10:00Z").as_deref(),
            Some("no run succeeded in 9, schedule `0 * * * *`")
        );
        // Every minute for days: counted up to a bound.
        let job =
            cronjob("* * * * *", "2026-01-01T00:00:00Z", Some("2026-11-24T09:59:00Z"), Some("2026-11-20T00:00:00Z"));
        assert!(reason(&job, "2026-11-24T10:00:00Z").unwrap().contains("100+ runs since"));
    }

    #[test]
    fn the_schedule_follows_its_time_zone() {
        if TimeZone::get("Europe/Paris").is_err() {
            eprintln!("no time zone database: skipped");
            return;
        }
        // 02:00 in Paris is 01:00 UTC in November: the 23rd's run is due at 01:00 UTC.
        let mut job =
            cronjob("0 2 * * *", "2026-01-01T00:00:00Z", Some("2026-11-21T01:00:00Z"), Some("2026-11-21T01:05:00Z"));
        job.spec.time_zone = Some("Europe/Paris".into());
        assert_eq!(
            reason(&job, "2026-11-23T01:01:01Z").as_deref(),
            Some("no run for 2d, schedule `0 2 * * *` (Europe/Paris)")
        );
        assert_eq!(reason(&job, "2026-11-23T01:00:30Z"), None);
        // The older way, in the schedule.
        let mut job = cronjob(
            "CRON_TZ=Europe/Paris 0 2 * * *",
            "2026-01-01T00:00:00Z",
            Some("2026-11-21T01:00:00Z"),
            Some("2026-11-21T01:05:00Z"),
        );
        assert!(reason(&job, "2026-11-23T01:01:01Z").unwrap().ends_with("(Europe/Paris)"));
        // spec.timeZone wins over the prefix.
        job.spec.time_zone = Some("UTC".into());
        assert_eq!(reason(&job, "2026-11-23T01:01:01Z"), None);
    }

    #[test]
    fn an_unknown_time_zone_shows_only_when_it_never_ran() {
        let mut job = cronjob("0 2 * * *", "2026-11-24T08:00:00Z", None, None);
        job.spec.time_zone = Some("Nowhere/Never".into());
        assert_eq!(reason(&job, "2026-11-24T08:30:00Z"), None);
        assert_eq!(
            reason(&job, "2026-11-24T09:30:00Z").as_deref(),
            Some("never ran: its time zone `Nowhere/Never` is unknown")
        );
        job.status.last_schedule_time = Some("2026-11-24T08:10:00Z".into());
        assert_eq!(reason(&job, "2026-11-30T09:30:00Z"), None);
    }

    #[test]
    fn a_schedule_that_does_not_read_claims_nothing() {
        for schedule in ["@every 1h", "", "not a schedule"] {
            let job = cronjob(schedule, "2020-01-01T00:00:00Z", None, None);
            assert_eq!(reason(&job, "2026-11-24T10:00:00Z"), None, "{schedule}");
        }
    }

    proptest! {
        /// A CronJob that last ran at the time its schedule last fired, and succeeded then,
        /// is never a problem, whatever the schedule and the time.
        #[test]
        fn a_cronjob_up_to_date_is_never_a_problem(
            minute in 0u32..60,
            hour in prop::option::of(0u32..24),
            weekday in prop::option::of(0u32..7),
            now in 1_700_000_000i64..1_900_000_000,
        ) {
            let schedule = format!(
                "{minute} {} * * {}",
                hour.map_or("*".into(), |h| h.to_string()),
                weekday.map_or("*".into(), |d| d.to_string())
            );
            let now = Timestamp::from_second(now).unwrap();
            let cron = Cron::parse(&schedule).unwrap();
            let last = cron.prev_before(&now.to_zoned_datetime(TimeZone::UTC)).unwrap().timestamp();
            let last = last.to_string();
            let job = cronjob(&schedule, "2020-01-01T00:00:00Z", Some(&last), Some(&last));
            prop_assert_eq!(judge(&job, now), None);
        }
    }
}
