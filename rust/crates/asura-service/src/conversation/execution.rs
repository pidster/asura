//! The conversation owner's execution setting (execution-setting.md, EX11):
//! what one persisted change does. Pure: it computes the projects turned
//! from on to off and the `execution.changed` events, one per changed
//! entry (ES-E7). The owner applies them in one reactor step.
use asura_storage::audit::{Event, ExecutionScope, ExecutionValue};
use asura_storage::authority::conversation::Id;
use asura_storage::config::ExecutionSetting;
use std::collections::BTreeSet;

/// One applied change.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Change {
    /// Projects whose effective value changed from on to off, in ID order:
    /// each gets a model-origin revocation with `execution_off`.
    pub revoked: Vec<Id>,
    /// The global event first, then one event per changed override in
    /// ascending project ID order.
    pub events: Vec<Event>,
}

fn value(entry: Option<bool>) -> ExecutionValue {
    match entry {
        Some(true) => ExecutionValue::On,
        Some(false) => ExecutionValue::Off,
        None => ExecutionValue::Inherit,
    }
}
fn switch(on: bool) -> ExecutionValue {
    if on {
        ExecutionValue::On
    } else {
        ExecutionValue::Off
    }
}

/// The change from `old` to `new`. `registered` are the projects the owner
/// knows; the projects named in either override map are considered too.
/// Each revoked project is counted in exactly one event's
/// `revoked_projects`: its own project event when its override changed,
/// otherwise the global event.
pub(super) fn change(
    old: &ExecutionSetting,
    new: &ExecutionSetting,
    registered: impl IntoIterator<Item = Id>,
) -> Change {
    let projects: BTreeSet<Id> = registered
        .into_iter()
        .chain(old.projects.keys().copied())
        .chain(new.projects.keys().copied())
        .collect();
    let revoked: Vec<Id> = projects
        .iter()
        .copied()
        .filter(|p| old.effective(p) && !new.effective(p))
        .collect();
    let override_changed = |p: &Id| old.project_override(p) != new.project_override(p);
    let mut events = Vec::new();
    if old.enabled != new.enabled {
        let counted = revoked.iter().filter(|p| !override_changed(p)).count();
        events.push(Event::ExecutionChanged {
            scope: ExecutionScope::Global,
            project: None,
            from: switch(old.enabled),
            to: switch(new.enabled),
            revoked_projects: u32::try_from(counted).unwrap_or(u32::MAX),
        });
    }
    for project in projects.iter().filter(|p| override_changed(p)) {
        events.push(Event::ExecutionChanged {
            scope: ExecutionScope::Project,
            project: Some(*project),
            from: value(old.project_override(project)),
            to: value(new.project_override(project)),
            revoked_projects: u32::from(revoked.contains(project)),
        });
    }
    Change { revoked, events }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn setting(enabled: bool, projects: &[(u8, bool)]) -> ExecutionSetting {
        ExecutionSetting {
            enabled,
            projects: projects
                .iter()
                .map(|(p, v)| ([*p; 16], *v))
                .collect::<BTreeMap<_, _>>(),
        }
    }
    fn counts(change: &Change) -> u32 {
        change
            .events
            .iter()
            .map(|e| match e {
                Event::ExecutionChanged {
                    revoked_projects, ..
                } => *revoked_projects,
                _ => 0,
            })
            .sum()
    }

    /// ES-11 (unit): a whole-mapping set gives one event per changed entry,
    /// global first, then projects in ID order; the counts sum to the
    /// revoked projects; every event passes validation.
    #[test]
    fn es_11_one_event_per_changed_entry_global_first() {
        let old = setting(true, &[(3, false), (5, true)]);
        let new = setting(false, &[(2, true), (5, false)]);
        let change = change(&old, &new, [[1; 16], [2; 16], [4; 16]]);
        // 1 and 4 inherit off; 5's override turned off; 2 stays on; 3 was off.
        assert_eq!(change.revoked, vec![[1; 16], [4; 16], [5; 16]]);
        let scopes: Vec<_> = change
            .events
            .iter()
            .map(|e| match e {
                Event::ExecutionChanged {
                    scope,
                    project,
                    from,
                    to,
                    revoked_projects,
                } => (*scope, *project, *from, *to, *revoked_projects),
                _ => unreachable!(),
            })
            .collect();
        use ExecutionValue::*;
        assert_eq!(
            scopes,
            vec![
                (ExecutionScope::Global, None, On, Off, 2),
                (ExecutionScope::Project, Some([2; 16]), Inherit, On, 0),
                (ExecutionScope::Project, Some([3; 16]), Off, Inherit, 0),
                (ExecutionScope::Project, Some([5; 16]), On, Off, 1),
            ]
        );
        assert_eq!(counts(&change) as usize, change.revoked.len());
        for event in &change.events {
            assert!(event.validate().is_ok(), "{event:?}");
        }
    }
    /// ES-03 and ES-07: an override beats the global default; off to on
    /// revokes nothing; a set that changes nothing gives no event.
    #[test]
    fn es_07_off_to_on_revokes_nothing_and_no_change_gives_no_event() {
        let old = setting(false, &[(1, true)]);
        assert!(old.effective(&[1; 16]) && !old.effective(&[2; 16]));
        let new = setting(true, &[(1, true)]);
        let change = change(&old, &new, [[1; 16], [2; 16]]);
        assert!(change.revoked.is_empty());
        assert_eq!(change.events.len(), 1);
        assert_eq!(
            super::change(&new, &new, [[1; 16]]),
            Change::default(),
            "no change, no event"
        );
        // A project override to off revokes that project only (ES-05).
        let off = setting(true, &[(1, false)]);
        let change = super::change(&new, &off, [[1; 16], [2; 16]]);
        assert_eq!(change.revoked, vec![[1; 16]]);
        assert_eq!(counts(&change), 1);
    }
}
