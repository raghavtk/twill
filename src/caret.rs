//! Shared, event-driven cursor blinking for the editor and terminal.
#[derive(Clone, Copy, Default)]
struct Blink {
    started: f64,
    focused: bool,
}
impl Blink {
    fn update(&mut self, now: f64, focused: bool, activity: bool) -> (bool, f64) {
        if focused && (!self.focused || activity) {
            self.started = now;
        }
        self.focused = focused;
        let phase = (now - self.started).rem_euclid(1.0);
        (focused && phase < 0.5, 0.5 - phase % 0.5)
    }
}

pub fn visible(ctx: &egui::Context, id: egui::Id, focused: bool) -> bool {
    let (now, focused, activity) = ctx.input(|input| {
        let activity = input.pointer.any_pressed()
            || input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key { pressed: true, .. }
                        | egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Ime(egui::ImeEvent::Commit(_))
                )
            });
        (input.time, focused && input.focused, activity)
    });
    let (visible, delay) = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Blink>(id.with("caret-blink"))
            .update(now, focused, activity)
    });
    if focused {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(delay));
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blinks_at_half_second_intervals_and_resets_on_input_or_focus() {
        let mut blink = Blink::default();
        assert!(blink.update(2.0, true, false).0);
        assert!(blink.update(2.49, true, false).0);
        assert!(!blink.update(2.5, true, false).0);
        assert!(blink.update(3.0, true, false).0);
        assert!(blink.update(3.6, true, true).0);
        assert!(!blink.update(4.2, true, false).0);
        assert!(!blink.update(4.3, false, false).0);
        assert!(blink.update(4.4, true, false).0);
    }
}
