//! Backend-independent player controls built with egui.

pub use egui::{Context as GuiContext, FullOutput as GuiFrame};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlayerGuiAction {
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
    pub status: Option<&'a str>,
    pub diagnostics: &'a str,
}

#[derive(Default)]
pub struct PlayerGui {
    context: GuiContext,
    input: egui::RawInput,
    diagnostics_visible: bool,
    control_rects: [Option<egui::Rect>; 2],
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
    }

    pub fn reset_renderer(&mut self) {
        self.context = GuiContext::default();
    }

    pub fn frame(&mut self, view: PlayerGuiView<'_>) -> (GuiFrame, Option<PlayerGuiAction>) {
        let mut action = None;
        let mut diagnostics_visible = self.diagnostics_visible;
        let mut control_rects = self.control_rects;
        let context = self.context.clone();
        let output = context.run(std::mem::take(&mut self.input), |context| {
            egui::TopBottomPanel::top("player-controls")
                .frame(
                    egui::Frame::side_top_panel(&context.style())
                        .fill(egui::Color32::from_black_alpha(210)),
                )
                .show(context, |ui| {
                    ui.horizontal(|ui| {
                        let controls_width = 64.0 + ui.spacing().item_spacing.x;
                        ui.add_space(((ui.available_width() - controls_width) / 2.0).max(0.0));
                        let (clicked, rectangle) = fullscreen_button(ui, view.fullscreen);
                        control_rects[0] = Some(rectangle);
                        if clicked {
                            action = Some(PlayerGuiAction::ToggleFullscreen);
                        }
                        let (clicked, rectangle) = diagnostics_button(ui, diagnostics_visible);
                        control_rects[1] = Some(rectangle);
                        if clicked {
                            diagnostics_visible = !diagnostics_visible;
                        }
                    });
                });

            if let Some(status) = view.status {
                egui::Area::new("player-status".into())
                    .anchor(egui::Align2::LEFT_TOP, egui::vec2(12.0, 52.0))
                    .show(context, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.label(egui::RichText::new(status).monospace());
                        });
                    });
            }

            if diagnostics_visible {
                egui::Area::new("player-diagnostics".into())
                    .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-12.0, 52.0))
                    .show(context, |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(view.diagnostics).monospace())
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                    });
            }
        });
        self.diagnostics_visible = diagnostics_visible;
        self.control_rects = control_rects;
        (output, action)
    }
}

fn fullscreen_button(ui: &mut egui::Ui, fullscreen: bool) -> (bool, egui::Rect) {
    let response = ui.add_sized([32.0, 24.0], egui::Button::new(""));
    let stroke = egui::Stroke::new(1.5, ui.style().interact(&response).fg_stroke.color);
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
    let stroke = egui::Stroke::new(1.5, ui.style().interact(&response).fg_stroke.color);
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
            status: None,
            diagnostics: "diagnostics",
        });
        assert!(output.pixels_per_point > 0.0);
    }

    #[test]
    fn diagnostics_button_changes_local_visibility() {
        let mut gui = prepared_gui();
        click_center(&mut gui, 1);
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
    fn controls_are_centered_in_the_viewport() {
        let gui = prepared_gui();
        let left = gui.control_rects[0].unwrap();
        let right = gui.control_rects[1].unwrap();
        let center = (left.left() + right.right()) / 2.0;
        assert!((center - 640.0).abs() < 0.5, "control center was {center}");
    }

    #[test]
    fn only_control_buttons_capture_remote_pointer_input() {
        let gui = prepared_gui();
        let button_center = gui.control_rects[0].unwrap().center();
        assert!(gui.captures_pointer_at(button_center.x, button_center.y));
        assert!(!gui.captures_pointer_at(640.0, 360.0));
        assert!(!gui.captures_pointer_at(640.0, 10.0));
    }

    fn prepared_gui() -> PlayerGui {
        let mut gui = PlayerGui::default();
        gui.update_viewport(1280.0, 720.0, 1.0, 0.0);
        let _ = gui.frame(view());
        gui.update_viewport(1280.0, 720.0, 1.0, 0.1);
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
            status: None,
            diagnostics: "diagnostics",
        }
    }
}
