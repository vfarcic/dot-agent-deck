//! PRD #1666 part 3, round 5 (plan D15): state with its own torn latch.
//!
//! The inbox hub's existing state recovers after a panic: every hub path
//! carries on with whatever a panicking thread left, as it always has. So the
//! hub mutex's poison flag cannot say whether the state this round adds is
//! half-written, because any hub panic sets it. [`Guarded`] keeps that state
//! apart, with a latch of its own.
//!
//! The value is reachable only through [`Guarded::section`] and
//! [`Guarded::read`], each of which lends it to a closure for the length of
//! the call: no reference to it can outlive the closure, and nothing else in
//! the crate can reach it, because its fields are private to this module.
//! A section takes the value out and marks the latch torn before it runs its
//! closure, and puts it back and clears the latch after the closure returns.
//! A closure that panics never returns, so the latch stays torn for the rest
//! of the run, and both accessors answer [`Torn`] from then on. That is
//! exactly "a panic happened inside a section that was writing it", and a
//! panic anywhere else does not set it.
//!
//! While a section runs, the value is lent out, so a nested `section` or
//! `read` on the same `Guarded` answers [`Torn`]: code inside a section must be
//! handed the value it already holds instead of reaching for it again. A
//! closure must not catch a panic of its own and return normally, which would
//! clear the latch over a half-written value.

/// The guarded state is torn: a panic happened inside a section that was
/// writing it (or it is lent to a section that is still running).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Torn;

/// A value reachable only through closures, with a torn latch. See the module
/// documentation.
#[derive(Debug)]
pub struct Guarded<T> {
    value: Option<T>,
    torn: bool,
}

impl<T> Guarded<T> {
    pub fn new(value: T) -> Self {
        Guarded {
            value: Some(value),
            torn: false,
        }
    }

    /// Run `f` on the value and an owner it lives in, as one section: the
    /// latch is set before `f` runs and cleared only when `f` returns.
    /// `field` names where in `owner` the guarded value is, so `f` can be
    /// handed both `owner` and the value at once.
    pub fn section<S, R>(
        owner: &mut S,
        field: impl Fn(&mut S) -> &mut Guarded<T>,
        f: impl FnOnce(&mut S, &mut T) -> R,
    ) -> Result<R, Torn> {
        let mut value = {
            let guarded = field(owner);
            if guarded.torn {
                return Err(Torn);
            }
            let Some(value) = guarded.value.take() else {
                return Err(Torn);
            };
            guarded.torn = true;
            value
        };
        let result = f(owner, &mut value);
        let guarded = field(owner);
        guarded.value = Some(value);
        guarded.torn = false;
        Ok(result)
    }

    /// Read the value, unless a section tore it.
    pub fn read<R>(&self, f: impl FnOnce(&T) -> R) -> Result<R, Torn> {
        match (&self.value, self.torn) {
            (Some(value), false) => Ok(f(value)),
            _ => Err(Torn),
        }
    }

    pub fn is_torn(&self) -> bool {
        self.torn
    }
}

#[cfg(test)]
mod tests {
    use spec::spec;

    use super::*;

    struct Owner {
        other: u32,
        guarded: Guarded<Vec<u32>>,
    }

    fn owner() -> Owner {
        Owner {
            other: 0,
            guarded: Guarded::new(vec![1]),
        }
    }

    /// Scenario: A section that returns normally writes the value and leaves the latch clear; a section whose closure panics leaves the latch torn, and every later section and read answers `Torn` while the rest of the owner stays usable.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_a_panic_inside_a_section_tears_the_latch_for_good() {
        let mut o = owner();
        Guarded::section(
            &mut o,
            |o| &mut o.guarded,
            |o, v| {
                o.other += 1;
                v.push(2);
            },
        )
        .unwrap();
        assert_eq!(o.guarded.read(|v| v.clone()), Ok(vec![1, 2]));
        assert!(!o.guarded.is_torn());

        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = Guarded::section(
                &mut o,
                |o| &mut o.guarded,
                |_, v| {
                    v.push(3);
                    panic!("inside the section");
                },
            );
        }));
        assert!(panicked.is_err());
        assert!(o.guarded.is_torn());
        assert_eq!(o.guarded.read(|v| v.len()), Err(Torn));
        assert_eq!(
            Guarded::section(&mut o, |o| &mut o.guarded, |_, v| v.len()),
            Err(Torn)
        );
        o.other += 1;
        assert_eq!(o.other, 2, "the rest of the owner is untouched");
    }

    /// Scenario: A panic outside any section — in code that holds the owner but not the guarded value — leaves the latch clear, and the guarded value readable.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_a_panic_outside_a_section_does_not_tear() {
        let mut o = owner();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            o.other += 1;
            panic!("outside any section");
        }));
        assert!(panicked.is_err());
        assert!(!o.guarded.is_torn());
        assert_eq!(o.guarded.read(|v| v.clone()), Ok(vec![1]));
    }

    /// Scenario: Code inside a section reaches for the guarded value again, with a nested section and a nested read. Both answer `Torn`, since the value is lent out, and the outer section still completes and clears the latch.
    #[spec("orchestration/work-done/032")]
    #[test]
    fn work_done_032_a_nested_access_inside_a_section_is_refused() {
        let mut o = owner();
        let nested = Guarded::section(
            &mut o,
            |o| &mut o.guarded,
            |o, v| {
                v.push(9);
                (
                    o.guarded.read(|v| v.len()),
                    Guarded::section(o, |o| &mut o.guarded, |_, v| v.len()),
                )
            },
        )
        .unwrap();
        assert_eq!(nested, (Err(Torn), Err(Torn)));
        assert_eq!(o.guarded.read(|v| v.clone()), Ok(vec![1, 9]));
    }
}
