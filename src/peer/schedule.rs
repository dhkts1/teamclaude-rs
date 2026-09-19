//! When a lend grant is open for borrowing: a daily time-of-day window, an
//! optional day-of-week filter, always read in **this Mac's local time**.
//!
//! # Midnight-crossing windows
//!
//! `between = Some((22:00, 08:00))` means "22:00 tonight through 08:00
//! tomorrow", the natural reading of an overnight window, and the one
//! a daily window asks for. The window is charged to the day it **starts**:
//! a Friday-only schedule with that window is open Friday 22:00 through
//! Saturday 08:00, not Saturday night into Sunday. An equal `(start, end)`
//! pair is a zero-width window and is never open; an operator who wants
//! "open all day" leaves [`Schedule::between`] as `None` rather than writing
//! an equal pair.
//!
//! # Local time, not a stored offset
//!
//! There is no `tz` field: the schedule is evaluated against the offset this
//! *host* is in at the instant being checked, the same
//! `UtcOffset::local_offset_at(..).unwrap_or(UtcOffset::UTC)` pattern
//! `src/usage.rs`'s [`crate::usage::local_day`] already uses, DST-
//! correct per instant, and this crate's multithreaded-soundness guard (which
//! *can* make the lookup fail, depending on the platform and how many threads
//! have already started) falls back to UTC rather than refusing every lease
//! either way. Storing an offset on the grant would answer a different, wrong
//! question ("what did this Mac's clock read when the grant was written")
//! instead of the one an operator means ("is it currently within the window,
//! here").
//!
//! [`Schedule::contains`] itself is a thin offset-conversion wrapper around
//! [`Schedule::contains_at_local`], which does the actual decision and takes
//! an already-local reading, so every test below (and the property test in
//! `tests/peer_props.rs`) asserts this module's own arithmetic, never
//! whichever timezone the machine running the test happens to be in.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, Weekday};

/// A time of day, minute resolution. [`Self::new`] is the only constructor
/// and refuses anything outside `0..24` hours or `0..60` minutes, so an
/// `Hhmm` in hand is always a real time of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hhmm {
    hour: u8,
    minute: u8,
}

impl Hhmm {
    /// `None` if `hour >= 24` or `minute >= 60`.
    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        if hour < 24 && minute < 60 {
            Some(Self { hour, minute })
        } else {
            None
        }
    }

    /// `0..24`.
    pub fn hour(self) -> u8 {
        self.hour
    }

    /// `0..60`.
    pub fn minute(self) -> u8 {
        self.minute
    }

    /// `0..1440`, minutes since local midnight.
    pub fn minute_of_day(self) -> u16 {
        u16::from(self.hour) * 60 + u16::from(self.minute)
    }
}

impl fmt::Display for Hhmm {
    /// `HH:MM`, zero-padded: the form the operator typed and the form the
    /// peers file stores.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl FromStr for Hhmm {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (hour, minute) = raw
            .trim()
            .split_once(':')
            .ok_or_else(|| format!("{raw:?} is not a time of day: it needs HH:MM"))?;
        let hour = hour
            .parse::<u8>()
            .map_err(|err| format!("{hour:?} is not an hour: {err}"))?;
        let minute = minute
            .parse::<u8>()
            .map_err(|err| format!("{minute:?} is not a minute: {err}"))?;
        Self::new(hour, minute)
            .ok_or_else(|| format!("{raw:?} is not a time of day: hours are 0..24, minutes 0..60"))
    }
}

/// A daily window as one string, `"22:00-08:00"`, the form the `--between`
/// flag takes and the form the peers file stores.
///
/// A type rather than two `Hhmm` keys because the pair is one decision and is
/// meaningless half-written: a file carrying a start with no end would have to
/// be given a meaning by every reader, and they would not agree. Parsed at
/// deserialization, so everything downstream holds two real times of day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Between {
    pub start: Hhmm,
    pub end: Hhmm,
}

impl fmt::Display for Between {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

impl From<Between> for String {
    fn from(window: Between) -> Self {
        window.to_string()
    }
}

impl FromStr for Between {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (start, end) = raw
            .trim()
            .split_once('-')
            .ok_or_else(|| format!("{raw:?} is not a window: it needs HH:MM-HH:MM"))?;
        Ok(Self {
            start: start.parse()?,
            end: end.parse()?,
        })
    }
}

impl TryFrom<String> for Between {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

/// The days a window may start on, as one comma-separated string,
/// `"mon,tue,wed"`, the form the `--days` flag takes and the form the peers
/// file stores.
///
/// Stored as a `Vec` in file order rather than a set, so a re-save hands the
/// operator back the list they wrote instead of a reordering; duplicates are
/// dropped at parse, and [`Self::set`] is what the decision is made against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Days(Vec<Weekday>);

impl Days {
    /// The days as the set [`Schedule`] asks its question of.
    pub fn set(&self) -> HashSet<Weekday> {
        self.0.iter().copied().collect()
    }

    /// Empty means the operator named no day at all, which is a refusal to
    /// build a schedule from rather than a schedule that is never open.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The three-letter name of each day, in the order [`Days`] accepts them.
const DAY_NAMES: [(&str, Weekday); 7] = [
    ("mon", Weekday::Monday),
    ("tue", Weekday::Tuesday),
    ("wed", Weekday::Wednesday),
    ("thu", Weekday::Thursday),
    ("fri", Weekday::Friday),
    ("sat", Weekday::Saturday),
    ("sun", Weekday::Sunday),
];

impl fmt::Display for Days {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .0
            .iter()
            .map(|day| {
                DAY_NAMES
                    .iter()
                    .find(|(_, known)| known == day)
                    // Every `Weekday` is in the table above, so the fallback is
                    // unreachable; it prints rather than panicking because a
                    // `Display` that can abort is a worse failure than a name.
                    .map_or("???", |(name, _)| *name)
            })
            .collect();
        write!(f, "{}", names.join(","))
    }
}

impl From<Days> for String {
    fn from(days: Days) -> Self {
        days.to_string()
    }
}

impl FromStr for Days {
    type Err = String;

    /// Case-insensitive, and it accepts the full name too (`monday`), because
    /// an operator who writes the long form has not made a mistake. An
    /// unknown name is refused naming itself and the seven it could have been:
    /// silently dropping it would leave a schedule open on a day nobody chose.
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let mut days = Vec::new();
        for token in raw.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() {
                continue;
            }
            let found = DAY_NAMES
                .iter()
                .find(|(name, day)| token == *name || token == day.to_string().to_ascii_lowercase())
                .map(|(_, day)| *day)
                .ok_or_else(|| {
                    format!("{token:?} is not a day: name them mon, tue, wed, thu, fri, sat or sun")
                })?;
            if !days.contains(&found) {
                days.push(found);
            }
        }
        Ok(Self(days))
    }
}

impl TryFrom<String> for Days {
    type Error = String;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

/// When a lend grant is open. `None` on either field is "no restriction on
/// that axis", the default an operator gets by not asking, and identical to
/// every grant written before schedules existed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schedule {
    /// `(start, end)` in local time. `start < end` is a same-day window;
    /// `start > end` crosses midnight (see the module doc); `start == end`
    /// is zero-width and never open.
    pub between: Option<(Hhmm, Hhmm)>,
    /// The set of days the window may **start** on. `None` is every day.
    pub days: Option<HashSet<Weekday>>,
}

impl Schedule {
    /// Open at every instant, what a grant with no schedule at all means.
    pub fn always() -> Self {
        Self::default()
    }

    /// Build a schedule out of the two keys a grant stores, or `None` when it
    /// stores neither.
    ///
    /// `None` and `Some(Schedule::always())` are the same decision and this
    /// returns the first, so a caller can tell a grant that named no schedule
    /// from one that named a window: only the second is worth printing, and a
    /// refusal only ever comes from the second.
    ///
    /// A `days` list that parsed to nothing at all is treated as no day
    /// filter, not as "never open": the empty list cannot be written through
    /// the CLI (`--days` refuses a name it does not know) and reading it as a
    /// permanent refusal would switch an operator's lending off over a typo in
    /// a file they hand-edited.
    pub fn from_parts(between: Option<Between>, days: Option<&Days>) -> Option<Self> {
        let days = days.filter(|days| !days.is_empty()).map(Days::set);
        let between = between.map(|window| (window.start, window.end));
        if between.is_none() && days.is_none() {
            return None;
        }
        Some(Self { between, days })
    }

    /// True if `now` falls inside this schedule, evaluated in this Mac's
    /// local time (see the module doc for why there is no stored `tz`).
    ///
    /// Converts to local, then defers the whole decision to
    /// [`Self::contains_at_local`], which is what the property test and the
    /// unit tests below exercise directly, so those tests assert this
    /// module's own arithmetic and never the host machine's timezone.
    pub fn contains(&self, now: OffsetDateTime) -> bool {
        let offset = UtcOffset::local_offset_at(now).unwrap_or(UtcOffset::UTC);
        self.contains_at_local(now.to_offset(offset))
    }

    /// [`Self::contains`]'s decision, given a datetime whose `hour`/`minute`/
    /// `weekday` are ALREADY the local wall-clock reading, the offset
    /// carried by `local` itself is not consulted.
    pub fn contains_at_local(&self, local: OffsetDateTime) -> bool {
        let day = local.weekday();
        let minute = u16::from(local.hour()) * 60 + u16::from(local.minute());

        let day_open = |d: Weekday| self.days.as_ref().is_none_or(|days| days.contains(&d));

        match self.between {
            None => day_open(day),
            Some((start, end)) => {
                let (start_m, end_m) = (start.minute_of_day(), end.minute_of_day());
                match start_m.cmp(&end_m) {
                    std::cmp::Ordering::Less => {
                        day_open(day) && minute >= start_m && minute < end_m
                    }
                    std::cmp::Ordering::Greater => {
                        // Crosses midnight: today is either the TAIL of a
                        // window that started yesterday, or the HEAD of a
                        // window starting today.
                        (day_open(day.previous()) && minute < end_m)
                            || (day_open(day) && minute >= start_m)
                    }
                    std::cmp::Ordering::Equal => false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_window_round_trips_through_the_string_the_file_stores() {
        let window: Between = "22:00-08:00".parse().expect("a window parses");
        assert_eq!(window.start, Hhmm::new(22, 0).unwrap());
        assert_eq!(window.end, Hhmm::new(8, 0).unwrap());
        assert_eq!(window.to_string(), "22:00-08:00");
        assert_eq!(
            serde_json::from_str::<Between>("\"9:05-17:00\"").expect("json parses"),
            Between {
                start: Hhmm::new(9, 5).unwrap(),
                end: Hhmm::new(17, 0).unwrap(),
            }
        );
        // Zero-padded on the way out even when it was not on the way in.
        assert_eq!(
            serde_json::to_string(&Between {
                start: Hhmm::new(9, 5).unwrap(),
                end: Hhmm::new(17, 0).unwrap(),
            })
            .expect("json"),
            "\"09:05-17:00\""
        );
    }

    #[test]
    fn a_window_that_is_not_two_times_of_day_is_refused_naming_itself() {
        for bad in [
            "22:00",
            "22:00-",
            "25:00-08:00",
            "22:60-08:00",
            "ten-eleven",
        ] {
            let err = bad
                .parse::<Between>()
                .expect_err("this must not parse as a window");
            assert!(
                err.contains("HH:MM") || err.contains("hours are 0..24"),
                "a refusal must say what shape was wanted, got {err:?} for {bad:?}"
            );
        }
        // The one an operator is most likely to type names the part that was
        // wrong rather than the whole string.
        let err = "25:00-08:00"
            .parse::<Between>()
            .expect_err("an out-of-range hour must not parse");
        assert!(err.contains("25:00"), "it must name the hour: {err:?}");
    }

    #[test]
    fn days_parse_in_either_spelling_and_keep_the_order_written() {
        let days: Days = "fri,mon,Monday,TUESDAY".parse().expect("days parse");
        assert_eq!(days.to_string(), "fri,mon,tue");
        assert_eq!(
            days.set(),
            HashSet::from([Weekday::Friday, Weekday::Monday, Weekday::Tuesday])
        );
    }

    #[test]
    fn an_unknown_day_is_refused_rather_than_dropped() {
        let err = "mon,funday"
            .parse::<Days>()
            .expect_err("an unknown day must be refused");
        assert!(err.contains("funday"), "the refusal must name it: {err:?}");
    }

    #[test]
    fn from_parts_is_none_only_when_the_grant_named_nothing() {
        assert_eq!(Schedule::from_parts(None, None), None);
        let empty: Days = "".parse().expect("an empty list parses");
        assert_eq!(Schedule::from_parts(None, Some(&empty)), None);
        let window: Between = "22:00-08:00".parse().expect("a window parses");
        let built = Schedule::from_parts(Some(window), None).expect("a window is a schedule");
        assert_eq!(
            built.between,
            Some((Hhmm::new(22, 0).unwrap(), Hhmm::new(8, 0).unwrap()))
        );
        assert_eq!(built.days, None);
    }

    #[test]
    fn hhmm_new_refuses_out_of_range() {
        assert!(Hhmm::new(24, 0).is_none());
        assert!(Hhmm::new(0, 60).is_none());
        assert!(Hhmm::new(23, 59).is_some());
    }

    #[test]
    fn no_schedule_is_always_open() {
        let schedule = Schedule::always();
        assert!(schedule.contains_at_local(datetime!(2026-01-05 03:00 UTC)));
        assert!(schedule.contains_at_local(datetime!(2026-06-15 23:59 UTC)));
    }

    #[test]
    fn same_day_window_is_open_only_inside_its_bounds() {
        let schedule = Schedule {
            between: Some((Hhmm::new(9, 0).unwrap(), Hhmm::new(17, 0).unwrap())),
            days: None,
        };
        assert!(!schedule.contains_at_local(datetime!(2026-01-05 08:59 UTC)));
        assert!(schedule.contains_at_local(datetime!(2026-01-05 09:00 UTC)));
        assert!(schedule.contains_at_local(datetime!(2026-01-05 16:59 UTC)));
        assert!(!schedule.contains_at_local(datetime!(2026-01-05 17:00 UTC)));
    }

    /// The boundary the rule's own example turns on: 21:59 is
    /// outside a `between (22:00, 08:00)` window, 22:01 is inside.
    #[test]
    fn midnight_crossing_window_is_open_at_2201_and_closed_at_2159() {
        let schedule = Schedule {
            between: Some((Hhmm::new(22, 0).unwrap(), Hhmm::new(8, 0).unwrap())),
            days: None,
        };
        assert!(!schedule.contains_at_local(datetime!(2026-01-05 21:59 UTC)));
        assert!(schedule.contains_at_local(datetime!(2026-01-05 22:01 UTC)));
        // The tail end, past midnight, on the FOLLOWING calendar day.
        assert!(schedule.contains_at_local(datetime!(2026-01-06 07:59 UTC)));
        assert!(!schedule.contains_at_local(datetime!(2026-01-06 08:00 UTC)));
    }

    #[test]
    fn equal_start_and_end_is_never_open() {
        let schedule = Schedule {
            between: Some((Hhmm::new(10, 0).unwrap(), Hhmm::new(10, 0).unwrap())),
            days: None,
        };
        assert!(!schedule.contains_at_local(datetime!(2026-01-05 10:00 UTC)));
        assert!(!schedule.contains_at_local(datetime!(2026-01-05 00:00 UTC)));
    }

    /// A midnight-crossing window is charged to the day it STARTS: a
    /// Friday-only schedule opens Saturday's early-morning tail too, but not
    /// Sunday's.
    #[test]
    fn midnight_crossing_window_credits_the_starting_day() {
        let schedule = Schedule {
            between: Some((Hhmm::new(22, 0).unwrap(), Hhmm::new(8, 0).unwrap())),
            days: Some(HashSet::from([Weekday::Friday])),
        };
        // 2026-01-09 is a Friday; 2026-01-10 is Saturday; 2026-01-11 Sunday.
        assert!(schedule.contains_at_local(datetime!(2026-01-09 23:00 UTC)));
        assert!(schedule.contains_at_local(datetime!(2026-01-10 07:00 UTC)));
        assert!(!schedule.contains_at_local(datetime!(2026-01-11 07:00 UTC)));
        assert!(!schedule.contains_at_local(datetime!(2026-01-10 23:00 UTC)));
    }
}
