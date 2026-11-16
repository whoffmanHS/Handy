//! The calendar event a meeting belongs to, read from the Mac's calendars
//! through EventKit. Any account added to Apple Calendar is covered.
//!
//! Recording never waits on this: it runs on its own thread, and a denied or
//! unanswered permission just means the note gets the default title.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use log::{debug, info};
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKEntityType, EKEvent, EKEventStore, EKParticipant, EKParticipantType,
};
use objc2_foundation::{NSDate, NSError};

/// An event counts as "now" from this long before it starts.
const STARTS_WITHIN_SECS: f64 = 10.0 * 60.0;
/// How long the user gets to answer the calendar permission prompt.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// How many times, and how far apart, the calendar is asked right after
/// access is first granted.
const FIRST_LOAD_ATTEMPTS: usize = 6;
const FIRST_LOAD_WAIT: Duration = Duration::from_secs(1);
/// Addresses that mark an event as a call.
const MEETING_LINKS: [&str; 4] = [
    "teams.microsoft.com",
    "teams.live.com",
    "zoom.us",
    "meet.google.com",
];

#[derive(Clone, Debug, PartialEq)]
pub struct CalendarEvent {
    pub title: String,
    /// Everyone invited except the user, organiser first.
    pub attendees: Vec<String>,
}

struct Candidate {
    event: CalendarEvent,
    has_meeting_link: bool,
    /// Seconds until it starts; negative once it has.
    starts_in: f64,
}

/// The event happening now, asking for calendar access the first time.
pub fn current_event() -> Option<CalendarEvent> {
    // SAFETY: EventKit's store may be used from any thread.
    let store = unsafe { EKEventStore::new() };
    let (store, just_granted) =
        match unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) } {
            EKAuthorizationStatus::FullAccess => (store, false),
            EKAuthorizationStatus::NotDetermined => {
                if !request_access(&store) {
                    info!("Calendar access wasn't granted; the meeting keeps its default title");
                    return None;
                }
                // A store made before access was granted sees no calendars.
                (unsafe { EKEventStore::new() }, true)
            }
            _ => {
                debug!("No calendar access; the meeting keeps its default title");
                return None;
            }
        };

    // Right after access is granted the calendars take a moment to load, so
    // an empty answer then is asked again a few times.
    let attempts = if just_granted { FIRST_LOAD_ATTEMPTS } else { 1 };
    let mut candidates = Vec::new();
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(FIRST_LOAD_WAIT);
        }
        candidates = events_now(&store);
        if !candidates.is_empty() {
            break;
        }
    }
    let event = pick(candidates);
    match &event {
        Some(event) => info!(
            "Meeting matched a calendar event with {} attendees",
            event.attendees.len()
        ),
        None => debug!("No calendar event is happening now"),
    }
    event
}

/// The events going on now or starting soon, all-day ones left out.
fn events_now(store: &EKEventStore) -> Vec<Candidate> {
    // SAFETY: plain reads on objects EventKit just handed over.
    unsafe {
        let from = NSDate::now();
        let to = NSDate::dateWithTimeIntervalSinceNow(STARTS_WITHIN_SECS);
        let predicate = store.predicateForEventsWithStartDate_endDate_calendars(&from, &to, None);
        store
            .eventsMatchingPredicate(&predicate)
            .iter()
            .filter(|event| !event.isAllDay())
            .filter_map(|event| candidate(&event))
            .collect()
    }
}

fn request_access(store: &EKEventStore) -> bool {
    let (answer, answered) = mpsc::channel();
    let completion = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        let _ = answer.send(granted.as_bool());
    });
    // SAFETY: the block is kept alive until the answer arrives or times out,
    // and EventKit copies it for as long as it needs it.
    unsafe { store.requestFullAccessToEventsWithCompletion(RcBlock::as_ptr(&completion)) };
    answered.recv_timeout(PROMPT_TIMEOUT).unwrap_or(false)
}

/// # Safety
/// `event` must be a live event from an `EKEventStore`.
unsafe fn candidate(event: &EKEvent) -> Option<Candidate> {
    let title = event.title().to_string().trim().to_string();
    if title.is_empty() {
        return None;
    }
    let mut attendees: Vec<String> = Vec::new();
    let mut add = |person: &EKParticipant| {
        if person.isCurrentUser()
            || matches!(
                person.participantType(),
                EKParticipantType::Room | EKParticipantType::Resource
            )
        {
            return;
        }
        if let Some(name) = person.name().map(|name| name.to_string()) {
            let name = name.trim().to_string();
            if !name.is_empty() && !attendees.contains(&name) {
                attendees.push(name);
            }
        }
    };
    if let Some(organizer) = event.organizer() {
        add(&organizer);
    }
    if let Some(invited) = event.attendees() {
        for person in invited.iter() {
            add(&person);
        }
    }

    let text = |value: Option<Retained<objc2_foundation::NSString>>| {
        value.map(|value| value.to_string()).unwrap_or_default()
    };
    let details = [
        text(event.notes()),
        text(event.location()),
        text(event.URL().and_then(|url| url.absoluteString())),
    ]
    .join("\n")
    .to_lowercase();
    Some(Candidate {
        event: CalendarEvent { title, attendees },
        has_meeting_link: MEETING_LINKS.iter().any(|link| details.contains(link)),
        starts_in: event.startDate().timeIntervalSinceNow(),
    })
}

/// An event with a call link wins; after that, the one starting closest to
/// now.
fn pick(candidates: Vec<Candidate>) -> Option<CalendarEvent> {
    candidates
        .into_iter()
        .min_by(|a, b| {
            b.has_meeting_link
                .cmp(&a.has_meeting_link)
                .then(a.starts_in.abs().total_cmp(&b.starts_in.abs()))
        })
        .map(|candidate| candidate.event)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(title: &str, has_meeting_link: bool, starts_in: f64) -> Candidate {
        Candidate {
            event: CalendarEvent {
                title: title.to_string(),
                attendees: Vec::new(),
            },
            has_meeting_link,
            starts_in,
        }
    }

    #[test]
    fn an_event_with_a_call_link_beats_one_without() {
        let picked = pick(vec![
            candidate("Focus block", false, -60.0),
            candidate("Kleckner sync", true, -1500.0),
        ]);
        assert_eq!(picked.unwrap().title, "Kleckner sync");
    }

    #[test]
    fn otherwise_the_event_starting_closest_to_now_wins() {
        let picked = pick(vec![
            candidate("Ran long", true, -3000.0),
            candidate("Starting now", true, 120.0),
        ]);
        assert_eq!(picked.unwrap().title, "Starting now");
    }

    #[test]
    fn no_events_means_no_title() {
        assert!(pick(Vec::new()).is_none());
    }
}
