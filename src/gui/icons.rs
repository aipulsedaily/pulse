//! Painted icons & button primitives (D35, D23-D25) - zero-behavior split
//! from gui/mod.rs.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum Icon {
    Plus,
    Minus,
    /// Folder outline — the launcher's directory rows (the old titlebar
    /// new-folder button died with D13).
    Folder,
    Terminal,
    // Icon::Kill died with the bar's kill button (task #22): lifecycle
    // actions live on the sidebar row's context menu now.
    Close,
    Grid,
    Search,
    ChevronUp,
    ChevronDown,
    DensityComfortable,
    DensityCompact,
    WinMin,
    WinMax,
    WinRestore,
    /// Panel with a left column — the sidebar collapse/expand toggle.
    Sidebar,
    /// Two offset rounded rects — copy the command text (P2).
    Copy,
    /// A document with lines — copy the block's output text (P2).
    CopyLines,
    /// ¾ circle arc + arrowhead — re-run the command (P2).
    Rerun,
    /// Three stacked bars — the header blocks-panel toggle (P2).
    Blocks,
    /// Clock face (circle + two hands) — the strip history toggle (P4).
    History,
    /// `>_` prompt mark — launcher shell rows (selector §9).
    Shell,
    /// Four-point spark — launcher Claude rows (selector §9).
    ClaudeSpark,
    /// Diagonal pen — inline rename (§5.4, task #22).
    Pencil,
    /// Three horizontal lines with offset round knobs — the settings entry
    /// point (task #33 G5: reads as "tune" at any size; a gear needs 8+
    /// teeth to survive 14px).
    Sliders,
    /// Up arrow (shaft + head) — the #34 update-ready affordance in the
    /// sidebar row and collapsed rail. Reads at 11px.
    UpdateArrow,
}

pub(super) fn lerp_col(a: Color32, b: Color32, t: f32) -> Color32 {
    a.lerp_to_gamma(b, t.clamp(0.0, 1.0))
}

/// Draw an icon centered in `rect` from painter primitives — no glyph fonts (D35).
pub(super) fn draw_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    let stroke = Stroke::new(1.5, color);
    match icon {
        Icon::Plus => {
            painter.line_segment([c - Vec2::new(r, 0.0), c + Vec2::new(r, 0.0)], stroke);
            painter.line_segment([c - Vec2::new(0.0, r), c + Vec2::new(0.0, r)], stroke);
        }
        Icon::Minus => {
            painter.line_segment([c - Vec2::new(r, 0.0), c + Vec2::new(r, 0.0)], stroke);
        }
        Icon::Grid => {
            // 2×2 rounded squares.
            let g = r * 0.35;
            for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let cc = c + Vec2::new(sx * r * 0.5, sy * r * 0.5);
                let sq = Rect::from_center_size(cc, Vec2::splat(g * 2.0));
                painter.rect_stroke(sq, CornerRadius::same(1), stroke, StrokeKind::Inside);
            }
        }
        Icon::Search => {
            let ring = c - Vec2::new(r * 0.2, r * 0.2);
            painter.circle_stroke(ring, r * 0.6, stroke);
            let a = ring + Vec2::new(r * 0.45, r * 0.45);
            painter.line_segment([a, c + Vec2::new(r * 0.9, r * 0.9)], stroke);
        }
        Icon::ChevronUp => {
            painter.line_segment([c + Vec2::new(-r * 0.7, r * 0.35), c + Vec2::new(0.0, -r * 0.35)], stroke);
            painter.line_segment([c + Vec2::new(0.0, -r * 0.35), c + Vec2::new(r * 0.7, r * 0.35)], stroke);
        }
        Icon::ChevronDown => {
            painter.line_segment([c + Vec2::new(-r * 0.7, -r * 0.35), c + Vec2::new(0.0, r * 0.35)], stroke);
            painter.line_segment([c + Vec2::new(0.0, r * 0.35), c + Vec2::new(r * 0.7, -r * 0.35)], stroke);
        }
        Icon::DensityComfortable => {
            // Two tall bars with a gap — roomy rows.
            for sy in [-0.45, 0.45] {
                let bar = Rect::from_center_size(
                    c + Vec2::new(0.0, sy * r),
                    Vec2::new(r * 1.6, r * 0.5),
                );
                painter.rect_filled(bar, CornerRadius::same(1), color);
            }
        }
        Icon::DensityCompact => {
            // Three thin lines close together — dense rows.
            for sy in [-0.6, 0.0, 0.6] {
                painter.line_segment(
                    [c + Vec2::new(-r * 0.8, sy * r), c + Vec2::new(r * 0.8, sy * r)],
                    stroke,
                );
            }
        }
        Icon::Sidebar => {
            // Window outline with a left column, à la every modern sidebar toggle.
            let outer = Rect::from_center_size(c, Vec2::new(r * 1.9, r * 1.6));
            painter.rect_stroke(outer, CornerRadius::same(2), stroke, StrokeKind::Inside);
            let div_x = outer.min.x + outer.width() * 0.38;
            painter.line_segment(
                [Pos2::new(div_x, outer.min.y + 1.5), Pos2::new(div_x, outer.max.y - 1.5)],
                stroke,
            );
        }
        Icon::WinMin => {
            painter.line_segment(
                [Pos2::new(c.x - r, c.y), Pos2::new(c.x + r, c.y)],
                Stroke::new(1.2, color),
            );
        }
        Icon::WinMax => {
            let sq = Rect::from_center_size(c, Vec2::splat(r * 1.6));
            painter.rect_stroke(sq, CornerRadius::ZERO, Stroke::new(1.2, color), StrokeKind::Inside);
        }
        Icon::WinRestore => {
            let s = r * 1.4;
            let back = Rect::from_min_size(
                Pos2::new(c.x - s * 0.35, c.y - s * 0.5),
                Vec2::splat(s * 0.85),
            );
            let front = Rect::from_min_size(
                Pos2::new(c.x - s * 0.5, c.y - s * 0.35),
                Vec2::splat(s * 0.85),
            );
            painter.rect_stroke(back, CornerRadius::ZERO, Stroke::new(1.1, color), StrokeKind::Inside);
            painter.rect_filled(front, CornerRadius::ZERO, BG);
            painter.rect_stroke(front, CornerRadius::ZERO, Stroke::new(1.1, color), StrokeKind::Inside);
        }
        Icon::Folder => {
            let w = r * 1.6;
            let h = r * 1.1;
            let top = c.y - h * 0.6;
            let body = Rect::from_min_max(
                Pos2::new(c.x - w, top + h * 0.25),
                Pos2::new(c.x + w, top + h * 1.25),
            );
            painter.rect_stroke(body, CornerRadius::same(2), stroke, StrokeKind::Inside);
            // Tab.
            painter.line_segment(
                [
                    Pos2::new(c.x - w, top + h * 0.25),
                    Pos2::new(c.x - w * 0.35, top + h * 0.25),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(c.x - w * 0.35, top + h * 0.25),
                    Pos2::new(c.x - w * 0.1, top),
                ],
                stroke,
            );
        }
        Icon::Shell => {
            // `>_` prompt mark: chevron + baseline underscore.
            let s = r * 0.9;
            painter.line_segment(
                [c + Vec2::new(-s, -s * 0.55), c + Vec2::new(-s * 0.3, 0.0)],
                stroke,
            );
            painter.line_segment(
                [c + Vec2::new(-s * 0.3, 0.0), c + Vec2::new(-s, s * 0.55)],
                stroke,
            );
            painter.line_segment(
                [c + Vec2::new(0.0, s * 0.55), c + Vec2::new(s, s * 0.55)],
                stroke,
            );
        }
        Icon::ClaudeSpark => {
            // Four-point spark: + with a shorter ×.
            let s = r * 0.9;
            let d = s * 0.5;
            painter.line_segment([c - Vec2::new(0.0, s), c + Vec2::new(0.0, s)], stroke);
            painter.line_segment([c - Vec2::new(s, 0.0), c + Vec2::new(s, 0.0)], stroke);
            painter.line_segment([c - Vec2::new(d, d), c + Vec2::new(d, d)], stroke);
            painter.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], stroke);
        }
        Icon::Terminal => {
            let box_rect = rect.shrink(rect.width() * 0.08);
            painter.rect_stroke(box_rect, CornerRadius::same(4), stroke, StrokeKind::Inside);
            let p = box_rect.min + Vec2::new(box_rect.width() * 0.22, box_rect.height() * 0.34);
            let mid = p + Vec2::new(box_rect.width() * 0.22, box_rect.height() * 0.16);
            painter.line_segment([p, mid], stroke);
            painter.line_segment(
                [mid, p + Vec2::new(0.0, box_rect.height() * 0.32)],
                stroke,
            );
            painter.line_segment(
                [
                    box_rect.min + Vec2::new(box_rect.width() * 0.52, box_rect.height() * 0.66),
                    box_rect.min + Vec2::new(box_rect.width() * 0.78, box_rect.height() * 0.66),
                ],
                stroke,
            );
        }
        Icon::Close => {
            let d = r * 0.8;
            painter.line_segment([c - Vec2::splat(d), c + Vec2::splat(d)], stroke);
            painter.line_segment(
                [c + Vec2::new(-d, d), c + Vec2::new(d, -d)],
                stroke,
            );
        }
        Icon::Copy => {
            let s = r * 1.3;
            let back = Rect::from_min_size(
                Pos2::new(c.x - s * 0.75, c.y - s * 0.75),
                Vec2::splat(s * 1.05),
            );
            let front = Rect::from_min_size(
                Pos2::new(c.x - s * 0.3, c.y - s * 0.3),
                Vec2::splat(s * 1.05),
            );
            painter.rect_stroke(back, CornerRadius::same(2), stroke, StrokeKind::Inside);
            painter.rect_filled(front.shrink(0.75), CornerRadius::same(2), BG);
            painter.rect_stroke(front, CornerRadius::same(2), stroke, StrokeKind::Inside);
        }
        Icon::CopyLines => {
            let w = r * 1.5;
            let h = r * 1.9;
            let doc = Rect::from_center_size(c, Vec2::new(w, h));
            painter.rect_stroke(doc, CornerRadius::same(2), stroke, StrokeKind::Inside);
            for sy in [-0.22f32, 0.0, 0.22] {
                painter.line_segment(
                    [
                        Pos2::new(doc.min.x + w * 0.25, c.y + sy * h),
                        Pos2::new(doc.max.x - w * 0.25, c.y + sy * h),
                    ],
                    stroke,
                );
            }
        }
        Icon::Rerun => {
            // ¾ arc, gap at the top-right, arrowhead on the arc's clockwise
            // end pointing into the gap.
            let rad = r * 0.8;
            use std::f32::consts::{FRAC_PI_2, TAU};
            let start = -FRAC_PI_2 + TAU * 0.08;
            let pts: Vec<Pos2> = (0..=20)
                .map(|i| {
                    let t = start + i as f32 / 20.0 * TAU * 0.79;
                    Pos2::new(c.x + rad * t.cos(), c.y + rad * t.sin())
                })
                .collect();
            painter.add(egui::Shape::line(pts, stroke));
            let tip = Pos2::new(c.x + rad * start.cos(), c.y + rad * start.sin());
            painter.line_segment([tip, tip + Vec2::new(-r * 0.45, -r * 0.2)], stroke);
            painter.line_segment([tip, tip + Vec2::new(-r * 0.15, r * 0.45)], stroke);
        }
        Icon::Blocks => {
            for sy in [-0.62f32, 0.0, 0.62] {
                let bar = Rect::from_center_size(
                    c + Vec2::new(0.0, sy * r),
                    Vec2::new(r * 1.7, r * 0.42),
                );
                painter.rect_filled(bar, CornerRadius::same(1), color);
            }
        }
        Icon::History => {
            // Clock face: ring + minute hand up, hour hand right-down.
            painter.circle_stroke(c, r * 0.85, stroke);
            painter.line_segment([c, c + Vec2::new(0.0, -r * 0.5)], stroke);
            painter.line_segment([c, c + Vec2::new(r * 0.4, r * 0.18)], stroke);
        }
        Icon::Pencil => {
            // Diagonal pencil: shaft from the writing tip (bottom-left) to
            // the eraser end (top-right), plus a ferrule tick across the
            // shaft near the top.
            let s = r * 0.9;
            let tip = c + Vec2::new(-s, s);
            let cap = c + Vec2::new(s * 0.75, -s * 0.75);
            painter.line_segment([tip, cap], stroke);
            let back = cap + Vec2::new(-s * 0.3, s * 0.3);
            painter.line_segment(
                [back + Vec2::new(-s * 0.22, -s * 0.22), back + Vec2::new(s * 0.22, s * 0.22)],
                stroke,
            );
        }
        Icon::Sliders => {
            // Three rails with a filled knob each, knobs offset so the
            // glyph reads as "tune" (never a gear — G5).
            let w = r * 0.95;
            for (kx, dy) in [(0.35f32, -0.6f32), (-0.4, 0.0), (0.15, 0.6)] {
                let y = c.y + dy * r;
                painter.line_segment(
                    [Pos2::new(c.x - w, y), Pos2::new(c.x + w, y)],
                    stroke,
                );
                painter.circle_filled(Pos2::new(c.x + kx * w, y), 2.2, color);
            }
        }
        Icon::UpdateArrow => {
            let top = c - Vec2::new(0.0, r * 0.9);
            painter.line_segment([c + Vec2::new(0.0, r * 0.9), top], stroke);
            painter.line_segment([top, top + Vec2::new(-r * 0.6, r * 0.6)], stroke);
            painter.line_segment([top, top + Vec2::new(r * 0.6, r * 0.6)], stroke);
        }
    }
}

/// Panel-nav icon button that renders visibly disabled (TEXT_FAINT, no hover
/// fill) when `enabled` is false — still discoverable, hover text explains.
/// Returns true on click while enabled.
pub(super) fn nav_icon_button(
    ui: &mut egui::Ui,
    icon: Icon,
    enabled: bool,
    hover_enabled: &str,
    hover_disabled: &str,
) -> bool {
    if enabled {
        icon_button(ui, icon, false).on_hover_text(hover_enabled).clicked()
    } else {
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
        draw_icon(ui.painter(), rect.shrink(8.0), icon, TEXT_FAINT);
        resp.on_hover_text(hover_disabled);
        false
    }
}

/// Destructive-hover wash derived from DANGER (N2: the RGB was hand-inlined
/// at three sites — a palette change must never drift them silently). `t` is
/// the 0..1 hover animation; alpha ramps to 30.
pub(super) fn danger_wash(t: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(DANGER.r(), DANGER.g(), DANGER.b(), (30.0 * t) as u8)
}

/// 28x28 icon button (D25). `danger` gives it a red hover/glyph.
pub(super) fn icon_button(ui: &mut egui::Ui, icon: Icon, danger: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        let fill = if danger {
            danger_wash(t)
        } else {
            SURFACE_2.gamma_multiply(t)
        };
        painter.rect_filled(rect, CornerRadius::same(6), fill);
    }
    let rest = if danger { DANGER } else { TEXT_MUTED };
    let hot = if danger { DANGER_HOVER } else { TEXT };
    draw_icon(painter, rect.shrink(8.0), icon, lerp_col(rest, hot, t));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Painted window caption button (~46px, full titlebar height). `danger` gives
/// the close button a red hover fill (V1).
pub(super) fn caption_button(ui: &mut egui::Ui, rect: Rect, icon: Icon, danger: bool) -> egui::Response {
    let resp = ui.interact(rect, Id::new(("caption", rect.min.x as i32)), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.10);
    if t > 0.0 {
        let fill = if danger {
            DANGER.gamma_multiply(t)
        } else {
            SURFACE_2.gamma_multiply(t)
        };
        ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
    }
    let col = if danger && resp.hovered() {
        Color32::WHITE
    } else {
        lerp_col(TEXT_SECONDARY, TEXT, t)
    };
    let g = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
    draw_icon(ui.painter(), g, icon, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Tiny 18px transparent icon button for the sidebar footer (V-B).
pub(super) fn footer_glyph(ui: &mut egui::Ui, icon: Icon) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let col = lerp_col(TEXT_MUTED, TEXT_SECONDARY, t);
    draw_icon(ui.painter(), rect.shrink(2.0), icon, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Small right-anchored pill showing an unread burst count (V-B). `amber` tints
/// it as an attention badge; otherwise it's a quiet surface pill.
pub(super) fn burst_badge(painter: &egui::Painter, right_center: Pos2, label: &str, amber: bool) {
    let (fill, fg) = if amber {
        (ATTENTION, ON_ACCENT)
    } else {
        (SURFACE_3, TEXT_SECONDARY)
    };
    let galley = painter.layout_no_wrap(label.to_string(), FontId::proportional(10.0), fg);
    let w = (galley.size().x + 10.0).max(16.0);
    let h = 16.0;
    let rect = Rect::from_min_max(
        Pos2::new(right_center.x - w, right_center.y - h / 2.0),
        Pos2::new(right_center.x, right_center.y + h / 2.0),
    );
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        fg,
    );
}

/// Full-width ghost button with an optional leading icon (D22/D24).
pub(super) fn ghost_button(ui: &mut egui::Ui, width: f32, label: &str, icon: Option<Icon>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 30.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if resp.is_pointer_button_down_on() {
        painter.rect_filled(rect, CornerRadius::same(8), OV_PRESSED);
    } else if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(8), SURFACE_2.gamma_multiply(t));
    }
    let fg = lerp_col(TEXT_SECONDARY, TEXT, t);
    let mut x = rect.min.x + 10.0;
    if let Some(icon) = icon {
        let ir = Rect::from_min_size(Pos2::new(x, rect.center().y - 7.0), Vec2::splat(14.0));
        draw_icon(painter, ir, icon, fg);
        x += 20.0;
    }
    painter.text(
        Pos2::new(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.0),
        fg,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Accent-filled primary button (D23). `danger` swaps to danger fills; a
/// disabled button uses surface_2 / text_faint and does not sense clicks.
pub(super) fn primary_button(ui: &mut egui::Ui, label: &str, danger: bool, enabled: bool) -> egui::Response {
    let text_col = if enabled { ON_ACCENT } else { TEXT_FAINT };
    let galley =
        ui.painter()
            .layout_no_wrap(label.to_string(), FontId::proportional(13.0), text_col);
    let width = galley.size().x + 28.0;
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 32.0), sense);
    let (base, hover, pressed) = if danger {
        (DANGER, DANGER_HOVER, DANGER)
    } else {
        (ACCENT, ACCENT_HOVER, ACCENT_PRESSED)
    };
    let fill = if !enabled {
        SURFACE_2
    } else if resp.is_pointer_button_down_on() {
        pressed
    } else if resp.hovered() {
        hover
    } else {
        base
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    if enabled {
        // 1px top inner highlight (D23).
        painter.line_segment(
            [
                Pos2::new(rect.min.x + 2.0, rect.min.y + 0.5),
                Pos2::new(rect.max.x - 2.0, rect.min.y + 0.5),
            ],
            Stroke::new(1.0, Color32::from_white_alpha(38)),
        );
    }
    painter.galley(
        Pos2::new(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text_col,
    );
    resp
}

/// Transparent ghost button sized to its label (D24), used in dialog footers
/// and the header. `color` lets the header's Restore button use accent text.
pub(super) fn ghost_button_auto(ui: &mut egui::Ui, label: &str, color: Color32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_string(),
        FontId::proportional(13.0),
        color,
    );
    let width = galley.size().x + 24.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 32.0), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), 0.12);
    let painter = ui.painter();
    if t > 0.0 {
        painter.rect_filled(rect, CornerRadius::same(8), SURFACE_2.gamma_multiply(t));
    }
    painter.galley(
        Pos2::new(rect.center().x - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    resp
}
