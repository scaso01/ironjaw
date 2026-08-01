use crate::store::ProactiveStore;
use crate::types::ScheduleAction;
use chrono::{Datelike, NaiveDateTime, Timelike};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Minimal 5-field cron expression: min hour dom month dow
/// Supports: numbers, *, */N (step), ranges (N-M), lists (N,M,O)
#[derive(Debug, Clone)]
pub struct CronExpr {
    pub minutes: CronField,
    pub hours: CronField,
    pub dom: CronField,
    pub month: CronField,
    pub dow: CronField,
}

#[derive(Debug, Clone)]
pub enum CronField {
    Any,
    Values(Vec<u32>),
}

impl CronExpr {
    /// Parse a 5-field cron expression.
    pub fn parse(expr: &str) -> Option<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return None;
        }

        Some(Self {
            minutes: parse_field(parts[0], 0, 59)?,
            hours: parse_field(parts[1], 0, 23)?,
            dom: parse_field(parts[2], 1, 31)?,
            month: parse_field(parts[3], 1, 12)?,
            dow: parse_field(parts[4], 0, 6)?,
        })
    }

    /// Check if the expression matches the given datetime (to the minute).
    pub fn matches(&self, dt: &NaiveDateTime) -> bool {
        field_matches(&self.minutes, dt.minute())
            && field_matches(&self.hours, dt.hour())
            && field_matches(&self.dom, dt.day())
            && field_matches(&self.month, dt.month())
            && field_matches(&self.dow, dt.weekday().num_days_from_sunday())
    }

    /// Find the next datetime after `after` that matches this cron expression.
    /// Searches up to 366 days ahead to avoid infinite loops.
    pub fn next_after(&self, after: &NaiveDateTime) -> Option<NaiveDateTime> {
        // Start from the next minute
        let mut dt = *after + chrono::Duration::minutes(1);
        dt = dt.with_second(0).unwrap_or(dt);

        let limit = *after + chrono::Duration::days(366);

        while dt < limit {
            if self.matches(&dt) {
                return Some(dt);
            }
            dt += chrono::Duration::minutes(1);
        }

        None
    }
}

fn parse_field(s: &str, min: u32, max: u32) -> Option<CronField> {
    if s == "*" {
        return Some(CronField::Any);
    }

    // Step: */N
    if let Some(step_str) = s.strip_prefix("*/") {
        let step: u32 = step_str.parse().ok()?;
        if step == 0 || step > max {
            return None;
        }
        let values: Vec<u32> = (min..=max).step_by(step as usize).collect();
        return Some(CronField::Values(values));
    }

    // List: N,M,O (also handles single values and ranges within lists)
    let mut values = Vec::new();
    for part in s.split(',') {
        if let Some((start_s, end_s)) = part.split_once('-') {
            // Range: N-M
            let start: u32 = start_s.parse().ok()?;
            let end: u32 = end_s.parse().ok()?;
            if start > max || end > max || start > end {
                return None;
            }
            for v in start..=end {
                values.push(v);
            }
        } else {
            // Single value
            let v: u32 = part.parse().ok()?;
            if v < min || v > max {
                return None;
            }
            values.push(v);
        }
    }

    if values.is_empty() {
        return None;
    }

    values.sort();
    values.dedup();
    Some(CronField::Values(values))
}

fn field_matches(field: &CronField, value: u32) -> bool {
    match field {
        CronField::Any => true,
        CronField::Values(values) => values.contains(&value),
    }
}

/// Cron scheduler: checks every `interval_secs` for due schedules and fires them.
pub struct Scheduler {
    store: ProactiveStore,
    interval_secs: u64,
}

impl Scheduler {
    pub fn new(store: ProactiveStore, interval_secs: u64) -> Self {
        Self {
            store,
            interval_secs,
        }
    }

    /// Start the scheduler loop. Fires ScheduleAction messages through the channel.
    pub async fn run(&self, tx: mpsc::Sender<ScheduleAction>) {
        info!(interval_secs = self.interval_secs, "Scheduler started");

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(self.interval_secs)).await;

            let now = chrono::Utc::now().naive_utc();

            let schedules = match self.store.list_schedules(true).await {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "Failed to list schedules");
                    continue;
                }
            };

            for schedule in schedules {
                let cron = match CronExpr::parse(&schedule.cron_expr) {
                    Some(c) => c,
                    None => {
                        warn!(
                            schedule_id = %schedule.id,
                            cron_expr = %schedule.cron_expr,
                            "Invalid cron expression, skipping"
                        );
                        continue;
                    }
                };

                if cron.matches(&now) {
                    // Check if already ran this minute
                    if let Some(last) = schedule.last_run {
                        let diff = now - last;
                        if diff.num_seconds() < 60 {
                            continue;
                        }
                    }

                    info!(
                        schedule_id = %schedule.id,
                        name = %schedule.name,
                        "Schedule triggered"
                    );

                    let next = cron.next_after(&now);

                    if let Err(e) = self.store.update_last_run(&schedule.id, now, next).await {
                        error!(error = %e, "Failed to update last_run");
                    }

                    let action = ScheduleAction {
                        schedule_id: schedule.id.clone(),
                        action_type: schedule.action_type,
                        action_data: schedule.action_data.clone(),
                    };

                    if tx.send(action).await.is_err() {
                        warn!("Scheduler channel closed, stopping");
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn dt(year: i32, month: u32, day: u32, hour: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, min, 0)
            .unwrap()
    }

    #[test]
    fn test_parse_every_minute() {
        let cron = CronExpr::parse("* * * * *").unwrap();
        assert!(cron.matches(&dt(2026, 3, 3, 8, 0)));
        assert!(cron.matches(&dt(2026, 3, 3, 23, 59)));
    }

    #[test]
    fn test_parse_specific_time() {
        // 8:00 every day
        let cron = CronExpr::parse("0 8 * * *").unwrap();
        assert!(cron.matches(&dt(2026, 3, 3, 8, 0)));
        assert!(!cron.matches(&dt(2026, 3, 3, 8, 1)));
        assert!(!cron.matches(&dt(2026, 3, 3, 9, 0)));
    }

    #[test]
    fn test_parse_step() {
        // Every 5 minutes
        let cron = CronExpr::parse("*/5 * * * *").unwrap();
        assert!(cron.matches(&dt(2026, 3, 3, 8, 0)));
        assert!(cron.matches(&dt(2026, 3, 3, 8, 5)));
        assert!(cron.matches(&dt(2026, 3, 3, 8, 10)));
        assert!(!cron.matches(&dt(2026, 3, 3, 8, 1)));
        assert!(!cron.matches(&dt(2026, 3, 3, 8, 3)));
    }

    #[test]
    fn test_parse_range() {
        // Weekdays only (Mon-Fri = 1-5)
        let cron = CronExpr::parse("0 9 * * 1-5").unwrap();
        // 2026-03-02 is Monday
        assert!(cron.matches(&dt(2026, 3, 2, 9, 0)));
        // 2026-03-01 is Sunday
        assert!(!cron.matches(&dt(2026, 3, 1, 9, 0)));
    }

    #[test]
    fn test_parse_list() {
        // At minute 0 and 30
        let cron = CronExpr::parse("0,30 * * * *").unwrap();
        assert!(cron.matches(&dt(2026, 3, 3, 8, 0)));
        assert!(cron.matches(&dt(2026, 3, 3, 8, 30)));
        assert!(!cron.matches(&dt(2026, 3, 3, 8, 15)));
    }

    #[test]
    fn test_parse_invalid() {
        assert!(CronExpr::parse("").is_none());
        assert!(CronExpr::parse("* * *").is_none());
        assert!(CronExpr::parse("60 * * * *").is_none());
        assert!(CronExpr::parse("* 25 * * *").is_none());
        assert!(CronExpr::parse("*/0 * * * *").is_none());
    }

    #[test]
    fn test_next_after() {
        let cron = CronExpr::parse("0 8 * * *").unwrap();
        let after = dt(2026, 3, 3, 7, 30);
        let next = cron.next_after(&after).unwrap();
        assert_eq!(next, dt(2026, 3, 3, 8, 0));
    }

    #[test]
    fn test_next_after_wraps_day() {
        let cron = CronExpr::parse("0 8 * * *").unwrap();
        let after = dt(2026, 3, 3, 9, 0);
        let next = cron.next_after(&after).unwrap();
        assert_eq!(next, dt(2026, 3, 4, 8, 0));
    }

    #[test]
    fn test_specific_dom_and_month() {
        // Jan 1st at midnight
        let cron = CronExpr::parse("0 0 1 1 *").unwrap();
        assert!(cron.matches(&dt(2026, 1, 1, 0, 0)));
        assert!(!cron.matches(&dt(2026, 1, 2, 0, 0)));
        assert!(!cron.matches(&dt(2026, 2, 1, 0, 0)));
    }
}
