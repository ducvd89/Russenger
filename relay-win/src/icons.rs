use eframe::egui::{self, vec2, Color32, CornerRadius, Rect, Shape, Stroke, StrokeKind};

#[derive(Clone, Copy)]
pub enum Icon {
    Chat,
    Bag,
    Mail,
    Archive,
    Search,
    Lock,
    Muted,
    Video,
    Phone,
    Attachment,
    Send,
    Check,
    Checks,
    Pending,
}

pub fn paint(painter: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let scale = rect.width().min(rect.height()) / 24.0;
    let point = |x, y| rect.center() + vec2((x - 12.0) * scale, (y - 12.0) * scale);
    let bounds = |x1, y1, x2, y2| Rect::from_min_max(point(x1, y1), point(x2, y2));
    let stroke = Stroke::new((scale * 1.8).max(1.1), color);
    let line = |points: &[(f32, f32)]| {
        painter.add(Shape::line(points.iter().map(|&(x, y)| point(x, y)).collect(), stroke));
    };
    let box_outline = |x1, y1, x2, y2, rounding| {
        painter.rect_stroke(bounds(x1, y1, x2, y2), CornerRadius::same(rounding), stroke, StrokeKind::Middle);
    };
    match icon {
        Icon::Chat => line(&[(3., 4.), (21., 4.), (21., 17.), (10., 17.), (5., 21.), (5., 17.), (3., 17.), (3., 4.)]),
        Icon::Bag => {
            box_outline(4., 7., 20., 21., 2);
            line(&[(8., 8.), (8., 5.), (10., 3.), (14., 3.), (16., 5.), (16., 8.)]);
        }
        Icon::Mail => {
            box_outline(3., 5., 21., 19., 2);
            line(&[(3., 6.), (12., 13.), (21., 6.)]);
        }
        Icon::Archive => {
            box_outline(3., 4., 21., 9., 1);
            line(&[(5., 9.), (5., 20.), (19., 20.), (19., 9.)]);
            line(&[(10., 13.), (14., 13.)]);
        }
        Icon::Search => {
            painter.circle_stroke(point(10., 10.), 6. * scale, stroke);
            line(&[(14.5, 14.5), (20., 20.)]);
        }
        Icon::Lock => {
            box_outline(5., 10., 19., 21., 2);
            line(&[(8., 10.), (8., 6.), (10., 3.5), (14., 3.5), (16., 6.), (16., 10.)]);
        }
        Icon::Muted => {
            line(&[(7., 6.), (7., 12.), (4., 16.), (20., 16.), (17., 12.), (17., 7.), (15., 4.), (11., 4.)]);
            line(&[(10., 20.), (14., 20.)]);
            line(&[(3., 3.), (21., 21.)]);
        }
        Icon::Video => {
            box_outline(3., 6., 15., 18., 2);
            line(&[(15., 10.), (21., 6.5), (21., 17.5), (15., 14.)]);
        }
        Icon::Phone => line(&[
            (5., 3.),
            (9., 7.),
            (7., 10.),
            (14., 17.),
            (17., 15.),
            (21., 19.),
            (18., 22.),
            (12., 20.),
            (5., 13.),
            (2., 7.),
            (5., 3.),
        ]),
        Icon::Attachment => line(&[
            (8., 13.),
            (15., 6.),
            (18., 6.),
            (19., 9.),
            (9., 19.),
            (5., 19.),
            (3., 15.),
            (4., 12.),
            (14., 2.),
        ]),
        Icon::Send => {
            line(&[(3., 4.), (21., 12.), (3., 20.), (7., 12.), (3., 4.)]);
            line(&[(7., 12.), (16., 12.)]);
        }
        Icon::Check => line(&[(4., 12.), (9., 17.), (20., 6.)]),
        Icon::Checks => {
            line(&[(2., 12.), (7., 17.), (17., 7.)]);
            line(&[(11., 15.), (13., 17.), (23., 7.)]);
        }
        Icon::Pending => {
            painter.circle_stroke(point(12., 12.), 8. * scale, stroke);
            line(&[(12., 7.), (12., 12.), (15., 14.)]);
        }
    }
}

pub fn button(ui: &mut egui::Ui, icon: Icon, enabled: bool, size: f32, color: Color32, label: &str) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let (rect, response) = ui.allocate_exact_size(vec2(size + 10.0, size + 8.0), egui::Sense::click());
        let icon_rect = Rect::from_center_size(rect.center(), vec2(size, size));
        let tint = if response.hovered() { ui.visuals().strong_text_color() } else { color };
        paint(ui.painter(), icon, icon_rect, if enabled { tint } else { tint.gamma_multiply(0.45) });
        response.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(label)
    })
    .inner
}
