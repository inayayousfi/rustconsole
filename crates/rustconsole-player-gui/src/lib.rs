//! Backend-independent player controls built with egui.

pub use egui::{Context as GuiContext, FullOutput as GuiFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerGuiAction {
    CapturePointer,
    ToggleFullscreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
    Extra1,
    Extra2,
}

#[derive(Clone, Copy, Debug)]
pub struct PlayerGuiView<'a> {
    pub fullscreen: bool,
    pub pointer_capture_available: bool,
    pub pointer_captured: bool,
    pub status: Option<&'a str>,
    pub diagnostics: &'a str,
}

#[derive(Default)]
pub struct PlayerGui {
    context: GuiContext,
    input: egui::RawInput,
    diagnostics_visible: bool,
    control_rects: [Option<egui::Rect>; 3],
    diagnostics_rect: Option<egui::Rect>,
}

impl PlayerGui {
    #[must_use]
    pub fn context(&self) -> GuiContext {
        self.context.clone()
    }

    #[must_use]
    pub const fn diagnostics_visible(&self) -> bool {
        self.diagnostics_visible
    }

    pub fn update_viewport(
        &mut self,
        logical_width: f32,
        logical_height: f32,
        pixels_per_point: f32,
        elapsed_seconds: f64,
    ) {
        self.context
            .set_pixels_per_point(pixels_per_point.max(f32::EPSILON));
        self.input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(logical_width.max(1.0), logical_height.max(1.0)),
        ));
        self.input.time = Some(elapsed_seconds);
    }

    pub fn pointer_moved(&mut self, x: f32, y: f32) {
        self.input
            .events
            .push(egui::Event::PointerMoved(egui::pos2(x, y)));
    }

    pub fn pointer_button(&mut self, x: f32, y: f32, button: PointerButton, pressed: bool) {
        self.input.events.push(egui::Event::PointerButton {
            pos: egui::pos2(x, y),
            button: match button {
                PointerButton::Primary => egui::PointerButton::Primary,
                PointerButton::Secondary => egui::PointerButton::Secondary,
                PointerButton::Middle => egui::PointerButton::Middle,
                PointerButton::Extra1 => egui::PointerButton::Extra1,
                PointerButton::Extra2 => egui::PointerButton::Extra2,
            },
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
    }

    pub fn pointer_gone(&mut self) {
        self.input.events.push(egui::Event::PointerGone);
    }

    pub fn mouse_wheel(&mut self, horizontal: f32, vertical: f32) {
        self.input.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(horizontal, vertical),
            modifiers: egui::Modifiers::NONE,
        });
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.input.focused = focused;
        self.input.events.push(egui::Event::WindowFocused(focused));
    }

    #[must_use]
    pub fn captures_pointer_at(&self, x: f32, y: f32) -> bool {
        let position = egui::pos2(x, y);
        self.control_rects
            .iter()
            .flatten()
            .any(|rectangle| rectangle.contains(position))
            || self
                .diagnostics_rect
                .is_some_and(|rectangle| rectangle.contains(position))
    }

    pub fn reset_renderer(&mut self) {
        self.context = GuiContext::default();
    }

    pub fn frame(&mut self, view: PlayerGuiView<'_>) -> (GuiFrame, Option<PlayerGuiAction>) {
        if view.pointer_captured {
            self.input.events.clear();
            self.control_rects = [None; 3];
            self.diagnostics_rect = None;
            return (
                GuiFrame {
                    pixels_per_point: self.context.pixels_per_point(),
                    ..GuiFrame::default()
                },
                None,
            );
        }

        let mut action = None;
        let mut diagnostics_visible = self.diagnostics_visible;
        let mut control_rects = self.control_rects;
        let mut diagnostics_rect = None;
        let context = self.context.clone();
        let output = context.run(std::mem::take(&mut self.input), |context| {
            let screen = context.content_rect();
            let controls_top = 24.0_f32.min((screen.height() - 48.0).max(0.0));
            let details_top = controls_top + 44.0;
            egui::Area::new("player-controls".into())
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, controls_top))
                .default_size(egui::vec2(124.0, 36.0))
                .movable(false)
                .show(context, |ui| {
                    egui::Frame::popup(ui.style())
                        .fill(egui::Color32::from_black_alpha(210))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (clicked, rectangle) = fullscreen_button(ui, view.fullscreen);
                                control_rects[0] = Some(rectangle);
                                if clicked {
                                    action = Some(PlayerGuiAction::ToggleFullscreen);
                                }
                                let (clicked, rectangle) = pointer_capture_button(
                                    ui,
                                    view.pointer_capture_available,
                                    view.pointer_captured,
                                );
                                control_rects[1] = Some(rectangle);
                                if clicked {
                                    action = Some(PlayerGuiAction::CapturePointer);
                                }
                                let (clicked, rectangle) =
                                    diagnostics_button(ui, diagnostics_visible);
                                control_rects[2] = Some(rectangle);
                                if clicked {
                                    diagnostics_visible = !diagnostics_visible;
                                }
                            });
                        });
                });

            if let Some(status) = view.status {
                egui::Area::new("player-status".into())
                    .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, details_top))
                    .movable(false)
                    .show(context, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(egui::RichText::new(status).monospace());
                        });
                    });
            }

            if diagnostics_visible {
                let panel_size = egui::vec2(
                    (screen.width() - 48.0).clamp(1.0, 600.0),
                    (screen.height() - details_top - 32.0).max(1.0),
                );
                let panel = egui::Area::new("player-diagnostics".into())
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, details_top))
                    .default_size(panel_size + egui::vec2(12.0, 12.0))
                    .movable(false)
                    .show(context, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            // Reserve the viewport before laying out scrollable content
                            // so the automatically sized area cannot collapse around it.
                            ui.set_min_size(panel_size);
                            ui.set_max_size(panel_size);
                            egui::ScrollArea::both()
                                .id_salt("diagnostics-scroll")
                                .auto_shrink([false, false])
                                .max_width(panel_size.x)
                                .max_height(panel_size.y)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(view.diagnostics).monospace(),
                                        )
                                        .wrap_mode(egui::TextWrapMode::Extend),
                                    );
                                });
                        });
                    });
                diagnostics_rect = Some(panel.response.rect);
            }
        });
        self.diagnostics_visible = diagnostics_visible;
        self.control_rects = control_rects;
        self.diagnostics_rect = diagnostics_rect;
        (output, action)
    }
}

fn fullscreen_button(ui: &mut egui::Ui, fullscreen: bool) -> (bool, egui::Rect) {
    let response = ui.add_sized([32.0, 24.0], egui::Button::new(""));
    let stroke = egui::Stroke::new(1.5_f32, ui.style().interact(&response).fg_stroke.color);
    let rectangle = response.rect.shrink(7.0);
    if fullscreen {
        let back = rectangle.translate(egui::vec2(2.0, -2.0));
        let front = rectangle.translate(egui::vec2(-2.0, 2.0));
        ui.painter()
            .rect_stroke(back, 0.0, stroke, egui::StrokeKind::Inside);
        ui.painter()
            .rect_stroke(front, 0.0, stroke, egui::StrokeKind::Inside);
    } else {
        let length = 4.0;
        let corners = [
            (
                rectangle.left_top(),
                egui::vec2(length, 0.0),
                egui::vec2(0.0, length),
            ),
            (
                rectangle.right_top(),
                egui::vec2(-length, 0.0),
                egui::vec2(0.0, length),
            ),
            (
                rectangle.left_bottom(),
                egui::vec2(length, 0.0),
                egui::vec2(0.0, -length),
            ),
            (
                rectangle.right_bottom(),
                egui::vec2(-length, 0.0),
                egui::vec2(0.0, -length),
            ),
        ];
        for (corner, horizontal, vertical) in corners {
            ui.painter()
                .line_segment([corner, corner + horizontal], stroke);
            ui.painter()
                .line_segment([corner, corner + vertical], stroke);
        }
    }
    let clicked = response.clicked();
    let rectangle = response.rect;
    response.on_hover_text(if fullscreen { "Windowed" } else { "Fullscreen" });
    (clicked, rectangle)
}

fn diagnostics_button(ui: &mut egui::Ui, visible: bool) -> (bool, egui::Rect) {
    let response = ui.add_sized([32.0, 24.0], egui::Button::new("").selected(visible));
    let stroke = egui::Stroke::new(1.5_f32, ui.style().interact(&response).fg_stroke.color);
    let rectangle = response.rect.shrink(7.0);
    ui.painter()
        .line_segment([rectangle.left_bottom(), rectangle.left_top()], stroke);
    ui.painter()
        .line_segment([rectangle.left_bottom(), rectangle.right_bottom()], stroke);
    ui.painter().line(
        vec![
            rectangle.left_bottom() + egui::vec2(2.0, -2.0),
            rectangle.center() + egui::vec2(-1.0, 2.0),
            rectangle.right_top() + egui::vec2(-1.0, 2.0),
        ],
        stroke,
    );
    let clicked = response.clicked();
    let rectangle = response.rect;
    response.on_hover_text(if visible {
        "Hide diagnostics"
    } else {
        "Show diagnostics"
    });
    (clicked, rectangle)
}

fn pointer_capture_button(
    ui: &mut egui::Ui,
    available: bool,
    captured: bool,
) -> (bool, egui::Rect) {
    let response = ui.add_enabled(
        available && !captured,
        egui::Button::new("")
            .selected(captured)
            .min_size(egui::vec2(32.0, 24.0)),
    );
    let stroke = egui::Stroke::new(1.5_f32, ui.style().interact(&response).fg_stroke.color);
    let rectangle = response.rect.shrink(7.0);
    ui.painter().circle_stroke(rectangle.center(), 5.0, stroke);
    ui.painter()
        .line_segment([rectangle.center_top(), rectangle.center_bottom()], stroke);
    ui.painter()
        .line_segment([rectangle.left_center(), rectangle.right_center()], stroke);
    let clicked = response.clicked();
    let rectangle = response.rect;
    response.on_hover_text(if !available {
        "Host pointer release unavailable"
    } else if captured {
        "Pointer anchored; release from the host tray"
    } else {
        "Anchor pointer"
    });
    (clicked, rectangle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_hidden_by_default() {
        assert!(!PlayerGui::default().diagnostics_visible());
    }

    #[test]
    fn viewport_scale_is_never_zero() {
        let mut gui = PlayerGui::default();
        gui.update_viewport(1280.0, 720.0, 0.0, 0.0);
        let (output, _) = gui.frame(PlayerGuiView {
            fullscreen: false,
            pointer_capture_available: true,
            pointer_captured: false,
            status: None,
            diagnostics: "diagnostics",
        });
        assert!(output.pixels_per_point > 0.0);
    }

    #[test]
    fn diagnostics_button_changes_local_visibility() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 2);
        let _ = gui.frame(view());
        assert!(gui.diagnostics_visible());
    }

    #[test]
    fn fullscreen_button_emits_an_action() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 0);
        let (_, action) = gui.frame(view());
        assert_eq!(action, Some(PlayerGuiAction::ToggleFullscreen));
    }

    #[test]
    fn pointer_button_emits_capture_action() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 1);
        let (_, action) = gui.frame(view());
        assert_eq!(action, Some(PlayerGuiAction::CapturePointer));
    }

    #[test]
    fn captured_input_disables_all_gui_output() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 0);
        let (output, action) = gui.frame(PlayerGuiView {
            pointer_captured: true,
            ..view()
        });
        assert!(output.shapes.is_empty());
        assert!(output.textures_delta.is_empty());
        assert_eq!(action, None);
        assert!(gui.control_rects.iter().all(Option::is_none));
    }

    #[test]
    fn unavailable_pointer_capture_emits_no_action() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 1);
        let (_, action) = gui.frame(PlayerGuiView {
            pointer_capture_available: false,
            ..view()
        });
        assert_eq!(action, None);
    }

    #[test]
    fn controls_are_centered_in_the_viewport() {
        let gui = prepared_gui();
        let left = gui.control_rects[0].unwrap();
        let right = gui.control_rects[2].unwrap();
        let center = (left.left() + right.right()) / 2.0;
        assert!((center - 640.0).abs() < 0.5, "control center was {center}");
        assert!(
            left.top() >= 24.0 && left.top() < 40.0,
            "buttons must have the selected top gap"
        );
    }

    #[test]
    fn only_control_buttons_capture_remote_pointer_input() {
        let gui = prepared_gui();
        let button_center = gui.control_rects[0].unwrap().center();
        assert!(gui.captures_pointer_at(button_center.x, button_center.y));
        assert!(!gui.captures_pointer_at(640.0, 360.0));
        assert!(!gui.captures_pointer_at(500.0, 10.0));
    }

    #[test]
    fn long_diagnostics_stay_on_screen_scroll_and_capture_pointer_input() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 2);
        let text = (0..100)
            .map(|line| format!("Diagnostic line {line}\n"))
            .collect::<String>();
        let view = PlayerGuiView {
            diagnostics: &text,
            ..view()
        };
        let _ = gui.frame(view);
        gui.update_viewport(1280.0, 720.0, 1.0, 0.3);
        let (before, _) = gui.frame(view);
        let panel = gui.diagnostics_rect.unwrap();
        assert!(panel.top() >= 68.0 && panel.top() < 80.0);
        assert!(panel.bottom() <= 720.0);
        assert!(
            panel.width() >= 600.0,
            "diagnostics must not collapse horizontally"
        );
        assert!(
            panel.height() >= 600.0,
            "long diagnostics need a usable scroll viewport"
        );
        let center = panel.center();
        assert!(gui.captures_pointer_at(center.x, center.y));
        assert!(!gui.captures_pointer_at(100.0, 360.0));
        let text_y = |frame: &GuiFrame| {
            frame
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::epaint::Shape::Text(text) = &shape.shape
                        && text.galley.job.text.starts_with("Diagnostic line 0")
                    {
                        return Some(text.pos.y);
                    }
                    None
                })
                .expect("diagnostics must produce text")
        };
        gui.pointer_moved(center.x, center.y);
        gui.update_viewport(1280.0, 720.0, 1.0, 0.35);
        let _ = gui.frame(view);
        gui.mouse_wheel(0.0, -5.0);
        gui.update_viewport(1280.0, 720.0, 1.0, 0.4);
        let _ = gui.frame(view);
        gui.update_viewport(1280.0, 720.0, 1.0, 0.45);
        let (after, _) = gui.frame(view);
        assert!(
            text_y(&after) < text_y(&before),
            "wheel input must scroll the diagnostics"
        );
        click_center(&mut gui, 2);
        let _ = gui.frame(view);
        assert!(
            !gui.captures_pointer_at(center.x, center.y),
            "hidden diagnostics must not block game input"
        );
    }

    fn prepared_gui() -> PlayerGui {
        let mut gui = PlayerGui::default();
        gui.update_viewport(1280.0, 720.0, 1.0, 0.0);
        let _ = gui.frame(view());
        gui.update_viewport(1280.0, 720.0, 1.0, 0.1);
        let _ = gui.frame(view());
        gui.update_viewport(1280.0, 720.0, 1.0, 0.2);
        gui
    }

    fn click_center(gui: &mut PlayerGui, index: usize) {
        let center = gui.control_rects[index].unwrap().center();
        gui.pointer_moved(center.x, center.y);
        gui.pointer_button(center.x, center.y, PointerButton::Primary, true);
        gui.pointer_button(center.x, center.y, PointerButton::Primary, false);
    }

    fn view() -> PlayerGuiView<'static> {
        PlayerGuiView {
            fullscreen: false,
            pointer_capture_available: true,
            pointer_captured: false,
            status: None,
            diagnostics: "diagnostics",
        }
    }
}
