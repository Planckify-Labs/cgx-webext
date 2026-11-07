//! `chrome.alarms` scheduling. Time is supplied by the caller, so this is deterministic and testable.

use serde_json::{Value, json};

/// Chrome 120+ allows alarms as short as 30 seconds; shorter requests are clamped.
pub const MIN_PERIOD_MINUTES: f64 = 0.5;

#[derive(Debug, Clone, PartialEq)]
pub struct Alarm {
    pub name: String,
    pub scheduled_time_ms: i64,
    pub period_minutes: Option<f64>,
}

impl Alarm {
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "name": self.name, "scheduledTime": self.scheduled_time_ms });
        if let Some(p) = self.period_minutes {
            v["periodInMinutes"] = json!(p);
        }
        v
    }
}

#[derive(Debug, Default)]
pub struct AlarmManager {
    alarms: Vec<Alarm>,
}

impl AlarmManager {
    /// `alarms.create(name, alarmInfo)`. An alarm with the same name is replaced.
    pub fn create(&mut self, name: &str, info: &Value, now_ms: i64) -> Result<(), String> {
        let obj = info.as_object().ok_or("alarmInfo must be an object")?;
        let when = obj.get("when").map(|v| v.as_f64().ok_or("'when' must be a number")).transpose()?;
        let delay = obj.get("delayInMinutes").map(|v| v.as_f64().ok_or("'delayInMinutes' must be a number")).transpose()?;
        let period = obj.get("periodInMinutes").map(|v| v.as_f64().ok_or("'periodInMinutes' must be a number")).transpose()?;

        if when.is_some() && delay.is_some() {
            return Err("Cannot set both 'when' and 'delayInMinutes'.".into());
        }
        if when.is_none() && delay.is_none() && period.is_none() {
            return Err("Must set at least one of 'when', 'delayInMinutes', or 'periodInMinutes'.".into());
        }
        if period.is_some_and(|p| p <= 0.0) {
            return Err("'periodInMinutes' must be greater than 0.".into());
        }
        if delay.is_some_and(|d| d < 0.0) {
            return Err("'delayInMinutes' must not be negative.".into());
        }
        let period = period.map(|p| p.max(MIN_PERIOD_MINUTES));

        let scheduled = match (when, delay, period) {
            (Some(w), _, _) => w as i64,
            (None, Some(d), _) => now_ms + (d * 60_000.0) as i64,
            (None, None, Some(p)) => now_ms + (p * 60_000.0) as i64,
            (None, None, None) => unreachable!("rejected above"),
        };
        self.alarms.retain(|a| a.name != name);
        self.alarms.push(Alarm { name: name.to_string(), scheduled_time_ms: scheduled, period_minutes: period });
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&Alarm> {
        self.alarms.iter().find(|a| a.name == name)
    }

    pub fn get_all(&self) -> &[Alarm] {
        &self.alarms
    }

    pub fn clear(&mut self, name: &str) -> bool {
        let before = self.alarms.len();
        self.alarms.retain(|a| a.name != name);
        self.alarms.len() != before
    }

    pub fn clear_all(&mut self) -> bool {
        let had = !self.alarms.is_empty();
        self.alarms.clear();
        had
    }

    /// Returns the alarms due at `now_ms`. One-shot alarms are removed; periodic ones are rescheduled one period from now.
    pub fn take_due(&mut self, now_ms: i64) -> Vec<Alarm> {
        let mut fired = vec![];
        let mut remaining = vec![];
        for mut alarm in std::mem::take(&mut self.alarms) {
            if alarm.scheduled_time_ms <= now_ms {
                fired.push(alarm.clone());
                if let Some(p) = alarm.period_minutes {
                    alarm.scheduled_time_ms = now_ms + (p * 60_000.0) as i64;
                    remaining.push(alarm);
                }
            } else {
                remaining.push(alarm);
            }
        }
        self.alarms = remaining;
        fired
    }

    /// When the next alarm is due, so the host can arm a single timer.
    pub fn next_due_ms(&self) -> Option<i64> {
        self.alarms.iter().map(|a| a.scheduled_time_ms).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_fires_once() {
        let mut m = AlarmManager::default();
        m.create("a", &json!({"delayInMinutes": 1}), 1000).unwrap();
        assert_eq!(m.get("a").unwrap().scheduled_time_ms, 61_000);
        assert_eq!(m.next_due_ms(), Some(61_000));
        assert!(m.take_due(60_999).is_empty());
        let fired = m.take_due(61_000);
        assert_eq!(fired.len(), 1);
        assert!(m.get("a").is_none());
        assert_eq!(m.next_due_ms(), None);
    }

    #[test]
    fn periodic_reschedules() {
        let mut m = AlarmManager::default();
        m.create("p", &json!({"delayInMinutes": 1, "periodInMinutes": 2}), 0).unwrap();
        assert_eq!(m.take_due(60_000).len(), 1);
        assert_eq!(m.get("p").unwrap().scheduled_time_ms, 60_000 + 120_000);
        assert_eq!(m.get("p").unwrap().to_json()["periodInMinutes"], json!(2.0));
    }

    #[test]
    fn period_only_starts_after_one_period_and_is_clamped() {
        let mut m = AlarmManager::default();
        m.create("p", &json!({"periodInMinutes": 0.1}), 0).unwrap();
        assert_eq!(m.get("p").unwrap().period_minutes, Some(MIN_PERIOD_MINUTES));
        assert_eq!(m.get("p").unwrap().scheduled_time_ms, 30_000);
    }

    #[test]
    fn when_is_absolute_and_past_fires_immediately() {
        let mut m = AlarmManager::default();
        m.create("w", &json!({"when": 500}), 10_000).unwrap();
        assert_eq!(m.take_due(10_000).len(), 1);
    }

    #[test]
    fn validation() {
        let mut m = AlarmManager::default();
        assert!(m.create("a", &json!({}), 0).is_err());
        assert!(m.create("a", &json!({"when": 1, "delayInMinutes": 1}), 0).is_err());
        assert!(m.create("a", &json!({"periodInMinutes": 0}), 0).is_err());
        assert!(m.create("a", &json!({"delayInMinutes": -1}), 0).is_err());
        assert!(m.create("a", &json!({"delayInMinutes": "x"}), 0).is_err());
        assert!(m.create("a", &json!(5), 0).is_err());
        assert!(m.get_all().is_empty());
    }

    #[test]
    fn replace_and_clear() {
        let mut m = AlarmManager::default();
        m.create("", &json!({"delayInMinutes": 1}), 0).unwrap();
        m.create("", &json!({"delayInMinutes": 5}), 0).unwrap();
        assert_eq!(m.get_all().len(), 1);
        assert_eq!(m.get("").unwrap().scheduled_time_ms, 300_000);
        m.create("b", &json!({"delayInMinutes": 1}), 0).unwrap();
        assert!(m.clear("b"));
        assert!(!m.clear("b"));
        assert!(m.clear_all());
        assert!(!m.clear_all());
    }
}
