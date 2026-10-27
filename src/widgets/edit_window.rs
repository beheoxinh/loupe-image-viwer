// Copyright (c) 2024-2026 Sophie Herold
// Copyright (c) 2025 Abhinandan Thakur
// Copyright (c) 2026 Maximiliano Sandoval
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::{Cell, OnceCell, RefCell};
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glycin::Operations;

use super::LpImage;
use super::{DrawFilter, DrawTool, LpDrawCanvas};
use crate::deps::*;
use crate::util::ErrorType;
use crate::util::gettext::*;
use crate::util::root::ParentWindow;

/// Whether the keyboard focus currently sits inside a text input, either an
/// editable widget or a text view. Walking the parents also catches editable
/// children such as the internal entry of a spin button.
fn focus_is_text_input(window: &super::LpEditWindow) -> bool {
    // The editor is an `AdwBin` hosted by the image window, so the focus
    // owner has to be looked up on the actual root window.
    let Some(root) = window.root().and_downcast::<gtk::Window>() else {
        return false;
    };
    let mut widget = gtk::prelude::GtkWindowExt::focus(&root);
    while let Some(current) = widget {
        if current.is::<gtk::Editable>() || current.is::<gtk::TextView>() {
            return true;
        }
        widget = current.parent();
    }
    false
}

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate, glib::Properties)]
    #[properties(wrapper_type = super::LpEditWindow)]
    #[template(file = "edit_window.ui")]
    pub struct LpEditWindow {
        #[template_child]
        toolbar_view: TemplateChild<adw::ToolbarView>,
        #[template_child]
        cancel: TemplateChild<gtk::Button>,
        #[template_child]
        pub(crate) save: TemplateChild<gtk::MenuButton>,
        #[template_child]
        saving_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        saving_status: TemplateChild<gtk::Label>,
        #[template_child]
        saving_info: TemplateChild<gtk::Revealer>,
        #[template_child]
        shortcut_controller: TemplateChild<gtk::ShortcutController>,

        #[template_child]
        mode_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        draw_toolbar: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        draw_statusbar: TemplateChild<gtk::Box>,
        #[template_child]
        draw_canvas: TemplateChild<LpDrawCanvas>,
        #[template_child]
        draw_color: TemplateChild<gtk::ColorDialogButton>,
        #[template_child]
        draw_width: TemplateChild<gtk::SpinButton>,
        #[template_child]
        draw_fill: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        draw_undo: TemplateChild<gtk::Button>,
        #[template_child]
        draw_redo: TemplateChild<gtk::Button>,
        #[template_child]
        draw_copy: TemplateChild<gtk::Button>,
        #[template_child]
        draw_cut: TemplateChild<gtk::Button>,
        #[template_child]
        draw_paste: TemplateChild<gtk::Button>,
        #[template_child]
        draw_filters: TemplateChild<gtk::MenuButton>,
        #[template_child]
        filter_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        tool_select: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_move: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        draw_clear: TemplateChild<gtk::Button>,
        #[template_child]
        recent_color_1: TemplateChild<gtk::Button>,
        #[template_child]
        recent_color_2: TemplateChild<gtk::Button>,
        #[template_child]
        recent_color_3: TemplateChild<gtk::Button>,
        #[template_child]
        zoom_out_btn: TemplateChild<gtk::Button>,
        #[template_child]
        zoom_fit_btn: TemplateChild<gtk::Button>,
        #[template_child]
        zoom_in_btn: TemplateChild<gtk::Button>,
        #[template_child]
        tool_pencil: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_brush: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_eraser: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_highlight: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_line: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_arrow: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_rectangle: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_round_rect: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_ellipse: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_text: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_bucket: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        tool_picker: TemplateChild<gtk::ToggleButton>,

        #[property(get, construct_only)]
        original_image: OnceCell<LpImage>,
        #[property(get, construct_only)]
        pub has_unsaved_changes: Cell<bool>,

        /// Whether the original file can be trashed
        pub(super) can_trash: Cell<bool>,
        /// Whether the drawing canvas is currently loading the original image
        pub(super) draw_loading: Cell<bool>,
        /// Whether a filter is running on a worker thread
        pub(super) filter_busy: Cell<bool>,

        pub(super) operations: RefCell<Option<Arc<Operations>>>,
        pub(super) recent_colors: RefCell<Vec<gdk::RGBA>>,
        pub(super) updating_color: Cell<bool>,
        save_cancellable: RefCell<Option<gio::Cancellable>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpEditWindow {
        const NAME: &'static str = "LpEditWindow";
        type Type = super::LpEditWindow;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            Self::bind_template(klass);

            klass.install_action("edit.save-copy", None, |obj, _, _| {
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    async move { obj.imp().save_copy().await }
                ));
            });

            klass.install_action("edit.save-overwrite", None, |obj, _, _| {
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    async move {
                        let _ = obj.imp().save_overwrite().await;
                    }
                ));
            });

            klass.install_action("edit.cancel", None, move |obj, _, _| {
                obj.imp().cancel();
            });
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for LpEditWindow {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                async move {
                    let can_trash = obj.original_image().can_trash().await;
                    obj.imp().can_trash.replace(can_trash);
                    obj.action_set_enabled("edit.save-overwrite", can_trash);
                }
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::S,
                    gdk::ModifierType::CONTROL_MASK,
                )),
                Some(gtk::NamedAction::new("edit.save-overwrite")),
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::S,
                    gdk::ModifierType::CONTROL_MASK.union(gdk::ModifierType::SHIFT_MASK),
                )),
                Some(gtk::NamedAction::new("edit.save-copy")),
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::Escape,
                    gdk::ModifierType::NO_MODIFIER_MASK,
                )),
                Some(gtk::NamedAction::new("edit.cancel")),
            ));

            self.setup_shortcuts();
            self.setup_draw();
        }
    }

    impl WidgetImpl for LpEditWindow {
        fn root(&self) {
            self.parent_root();

            self.update_save_state(false);
            self.start_drawing();
        }
    }
    impl BinImpl for LpEditWindow {}

    impl LpEditWindow {
        /// Editing shortcuts: undo/redo, clipboard and one mnemonic key per
        /// drawing tool.
        fn setup_shortcuts(&self) {
            let bind = |key: gdk::Key,
                            modifiers: gdk::ModifierType,
                            callback: Box<
                dyn Fn(&super::LpEditWindow, Option<&glib::Variant>) -> glib::Propagation
                    + 'static,
            >| {
                let action = gtk::CallbackAction::new(move |widget, args| {
                    let Some(window) = widget.downcast_ref::<super::LpEditWindow>() else {
                        return glib::Propagation::Proceed;
                    };
                    // These are global shortcuts: while a text field has focus
                    // (the text tool popover, the line width spin button, ...)
                    // bare letters must be typed and Ctrl+C/X/V/A must act on
                    // the text, not on the image.
                    if focus_is_text_input(window) {
                        return glib::Propagation::Proceed;
                    }
                    callback(window, args)
                });

                self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                    Some(gtk::KeyvalTrigger::new(key, modifiers)),
                    Some(action),
                ));
            };

            let canvas = self.draw_canvas.clone();

            bind(
                gdk::Key::z,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().draw_canvas.undo();
                    window.imp().update_draw_buttons();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::y,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().draw_canvas.redo();
                    window.imp().update_draw_buttons();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::z,
                gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
                Box::new(move |window, _| {
                    window.imp().draw_canvas.redo();
                    window.imp().update_draw_buttons();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::c,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().copy_to_clipboard();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::x,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().cut_to_clipboard();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::v,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().paste_from_clipboard();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::a,
                gdk::ModifierType::CONTROL_MASK,
                Box::new(move |window, _| {
                    window.imp().draw_canvas.select_all();
                    window.imp().update_draw_buttons();
                    glib::Propagation::Stop
                }),
            );

            bind(
                gdk::Key::Delete,
                gdk::ModifierType::NO_MODIFIER_MASK,
                Box::new(move |window, _| {
                    if window.imp().draw_canvas.delete_selection() {
                        window.imp().update_draw_buttons();
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                }),
            );

            bind(
                gdk::Key::Escape,
                gdk::ModifierType::NO_MODIFIER_MASK,
                Box::new(move |window, _| {
                    if window.imp().draw_canvas.has_selection() {
                        window.imp().draw_canvas.clear_selection();
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                }),
            );

            for (key, tool) in [
                (gdk::Key::p, DrawTool::Pencil),
                (gdk::Key::b, DrawTool::Brush),
                (gdk::Key::e, DrawTool::Eraser),
                (gdk::Key::h, DrawTool::Highlight),
                (gdk::Key::l, DrawTool::Line),
                (gdk::Key::a, DrawTool::Arrow),
                (gdk::Key::r, DrawTool::Rectangle),
                (gdk::Key::d, DrawTool::RoundedRectangle),
                (gdk::Key::o, DrawTool::Ellipse),
                (gdk::Key::t, DrawTool::Text),
                (gdk::Key::f, DrawTool::Bucket),
                (gdk::Key::i, DrawTool::Picker),
                (gdk::Key::s, DrawTool::Select),
                (gdk::Key::m, DrawTool::Move),
            ] {
                let canvas = canvas.clone();
                bind(
                    key,
                    gdk::ModifierType::NO_MODIFIER_MASK,
                    Box::new(move |window, _| {
                        canvas.set_tool(tool);
                        window.imp().sync_tool_buttons(tool);
                        glib::Propagation::Stop
                    }),
                );
            }

            for key in [gdk::Key::plus, gdk::Key::equal, gdk::Key::KP_Add] {
                let canvas = canvas.clone();
                bind(
                    key,
                    gdk::ModifierType::CONTROL_MASK,
                    Box::new(move |_, _| {
                        canvas.zoom_in();
                        glib::Propagation::Stop
                    }),
                );
            }

            for key in [gdk::Key::minus, gdk::Key::KP_Subtract] {
                let canvas = canvas.clone();
                bind(
                    key,
                    gdk::ModifierType::CONTROL_MASK,
                    Box::new(move |_, _| {
                        canvas.zoom_out();
                        glib::Propagation::Stop
                    }),
                );
            }

            for key in [gdk::Key::_0, gdk::Key::KP_0] {
                let canvas = canvas.clone();
                bind(
                    key,
                    gdk::ModifierType::CONTROL_MASK,
                    Box::new(move |_, _| {
                        canvas.zoom_reset();
                        glib::Propagation::Stop
                    }),
                );
            }

            drop(canvas);
        }

        fn select_recent_color(&self, index: usize) {
            let color = self.recent_colors.borrow().get(index).copied();
            let Some(color) = color else {
                return;
            };

            self.updating_color.set(true);
            self.draw_color.set_rgba(&color);
            self.draw_canvas.set_color(color);
            self.updating_color.set(false);
        }

        fn update_recent_swatches(&self) {
            let colors = self.recent_colors.borrow().clone();
            let buttons = [
                &self.recent_color_1,
                &self.recent_color_2,
                &self.recent_color_3,
            ];
            for (i, btn) in buttons.iter().enumerate() {
                if let Some(color) = colors.get(i) {
                    btn.set_visible(true);
                    btn.set_tooltip_text(Some(&format!(
                        "RGB({}, {}, {})",
                        (color.red() * 255.0).round() as u8,
                        (color.green() * 255.0).round() as u8,
                        (color.blue() * 255.0).round() as u8,
                    )));
                    if let Some(child) = btn.child() {
                        child.queue_draw();
                    }
                } else {
                    btn.set_visible(false);
                }
            }
        }

        fn add_recent_color(&self, color: gdk::RGBA) {
            if self.updating_color.get() {
                return;
            }

            let changed = {
                let mut recent = self.recent_colors.borrow_mut();
                if let Some(first) = recent.first() {
                    let diff = (first.red() - color.red()).abs()
                        + (first.green() - color.green()).abs()
                        + (first.blue() - color.blue()).abs()
                        + (first.alpha() - color.alpha()).abs();
                    if diff < 0.01 {
                        return;
                    }
                }
                recent.retain(|c| {
                    let diff = (c.red() - color.red()).abs()
                        + (c.green() - color.green()).abs()
                        + (c.blue() - color.blue()).abs()
                        + (c.alpha() - color.alpha()).abs();
                    diff >= 0.01
                });
                recent.insert(0, color);
                recent.truncate(3);
                true
            };

            if changed {
                self.update_recent_swatches();
            }
        }

        /// Reflect a tool change made from the keyboard in the sidebar.
        fn sync_tool_buttons(&self, tool: DrawTool) {
            for (button, candidate) in [
                (&self.tool_select, DrawTool::Select),
                (&self.tool_move, DrawTool::Move),
                (&self.tool_pencil, DrawTool::Pencil),
                (&self.tool_brush, DrawTool::Brush),
                (&self.tool_eraser, DrawTool::Eraser),
                (&self.tool_highlight, DrawTool::Highlight),
                (&self.tool_line, DrawTool::Line),
                (&self.tool_arrow, DrawTool::Arrow),
                (&self.tool_rectangle, DrawTool::Rectangle),
                (&self.tool_round_rect, DrawTool::RoundedRectangle),
                (&self.tool_ellipse, DrawTool::Ellipse),
                (&self.tool_text, DrawTool::Text),
                (&self.tool_bucket, DrawTool::Bucket),
                (&self.tool_picker, DrawTool::Picker),
            ] {
                if candidate == tool {
                    button.set_active(true);
                    return;
                }
            }
        }

        fn setup_draw(&self) {
            self.mode_stack.set_visible_child_name("draw");

            let tools = [
                (self.tool_select.clone(), DrawTool::Select),
                (self.tool_move.clone(), DrawTool::Move),
                (self.tool_pencil.clone(), DrawTool::Pencil),
                (self.tool_brush.clone(), DrawTool::Brush),
                (self.tool_eraser.clone(), DrawTool::Eraser),
                (self.tool_highlight.clone(), DrawTool::Highlight),
                (self.tool_line.clone(), DrawTool::Line),
                (self.tool_arrow.clone(), DrawTool::Arrow),
                (self.tool_rectangle.clone(), DrawTool::Rectangle),
                (self.tool_round_rect.clone(), DrawTool::RoundedRectangle),
                (self.tool_ellipse.clone(), DrawTool::Ellipse),
                (self.tool_text.clone(), DrawTool::Text),
                (self.tool_bucket.clone(), DrawTool::Bucket),
                (self.tool_picker.clone(), DrawTool::Picker),
            ];

            for (button, tool) in tools {
                if button.as_ptr() != self.tool_pencil.as_ptr() {
                    button.set_group(Some(&*self.tool_pencil));
                }
                button.connect_toggled(glib::clone!(
                    #[weak(rename_to = obj)]
                    self.obj(),
                    move |button| {
                        if button.is_active() {
                            obj.imp().draw_canvas.set_tool(tool);
                        }
                    }
                ));
            }

            let canvas = &self.draw_canvas;
            canvas.set_tool(DrawTool::Pencil);
            canvas.set_color(self.draw_color.rgba());
            canvas.set_line_width(self.draw_width.value());

            let color_dialog = gtk::ColorDialog::builder()
                .with_alpha(true)
                .title(gettext("Select Color"))
                .build();
            self.draw_color.set_dialog(&color_dialog);

            if self.recent_colors.borrow().is_empty() {
                *self.recent_colors.borrow_mut() = vec![
                    gdk::RGBA::BLACK,
                    gdk::RGBA::builder().red(0.86).green(0.15).blue(0.15).alpha(1.0).build(),
                    gdk::RGBA::builder().red(0.15).green(0.45).blue(0.86).alpha(1.0).build(),
                ];
            }

            for (i, btn) in [
                &self.recent_color_1,
                &self.recent_color_2,
                &self.recent_color_3,
            ]
            .iter()
            .enumerate()
            {
                let da = gtk::DrawingArea::builder()
                    .content_width(18)
                    .content_height(18)
                    .build();

                let weak_obj = self.obj().downgrade();
                da.set_draw_func(move |_, cr, width, height| {
                    let Some(obj) = weak_obj.upgrade() else {
                        return;
                    };
                    let color = obj.imp().recent_colors.borrow().get(i).copied();
                    let Some(color) = color else {
                        return;
                    };

                    cr.set_source_rgba(
                        color.red() as f64,
                        color.green() as f64,
                        color.blue() as f64,
                        color.alpha() as f64,
                    );
                    let r = 3.0;
                    let w = width as f64;
                    let h = height as f64;
                    cr.new_sub_path();
                    cr.arc(w - r, r, r, -std::f64::consts::FRAC_PI_2, 0.0);
                    cr.arc(w - r, h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
                    cr.arc(r, h - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
                    cr.arc(r, r, r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
                    cr.close_path();
                    let _ = cr.fill_preserve();
                    cr.set_source_rgba(0.0, 0.0, 0.0, 0.25);
                    cr.set_line_width(1.0);
                    let _ = cr.stroke();
                });
                btn.set_child(Some(&da));

                btn.connect_clicked(glib::clone!(
                    #[weak(rename_to = obj)]
                    self.obj(),
                    move |_| obj.imp().select_recent_color(i)
                ));
            }
            self.update_recent_swatches();

            self.draw_color.connect_rgba_notify(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |button| {
                    let imp = obj.imp();
                    if imp.updating_color.get() {
                        return;
                    }
                    let color = button.rgba();
                    imp.draw_canvas.set_color(color);
                    imp.add_recent_color(color);
                }
            ));
            self.draw_width.connect_value_changed(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |spin| obj.imp().draw_canvas.set_line_width(spin.value())
            ));
            self.draw_fill.connect_toggled(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |button| obj.imp().draw_canvas.set_fill_shape(button.is_active())
            ));
            self.draw_undo.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| {
                    obj.imp().draw_canvas.undo();
                    obj.imp().update_draw_buttons();
                }
            ));
            self.draw_redo.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| {
                    obj.imp().draw_canvas.redo();
                    obj.imp().update_draw_buttons();
                }
            ));
            self.draw_clear.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| {
                    obj.imp().draw_canvas.clear();
                    obj.imp().update_draw_buttons();
                }
            ));
            self.draw_copy.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().copy_to_clipboard()
            ));
            self.draw_cut.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().cut_to_clipboard()
            ));
            self.draw_paste.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().paste_from_clipboard()
            ));

            self.zoom_out_btn.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().draw_canvas.zoom_out()
            ));
            self.zoom_fit_btn.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().draw_canvas.zoom_reset()
            ));
            self.zoom_in_btn.connect_clicked(glib::clone!(
                #[weak(rename_to = obj)]
                self.obj(),
                move |_| obj.imp().draw_canvas.zoom_in()
            ));

            let weak_obj = self.obj().downgrade();
            canvas.connect_local("zoom-changed", false, move |values| {
                if let Some(obj) = weak_obj.upgrade()
                    && let Some(level) = values.get(1).and_then(|v| v.get::<f64>().ok())
                {
                    let pct = (level * 100.0).round() as i32;
                    obj.imp().zoom_fit_btn.set_label(&format!("{pct}%"));
                }
                None
            });

            self.setup_filters();

            let weak_obj = self.obj().downgrade();
            canvas.connect_local("selection-changed", false, move |_| {
                if let Some(obj) = weak_obj.upgrade() {
                    obj.imp().update_draw_buttons();
                }
                None
            });

            let weak_obj = self.obj().downgrade();
            canvas.connect_local("changed", false, move |_| {
                if let Some(obj) = weak_obj.upgrade() {
                    obj.imp().update_draw_buttons();
                    obj.imp().update_save_state(false);
                }
                None
            });

            let weak_obj = self.obj().downgrade();
            canvas.connect_local("color-picked", false, move |values| {
                if let Some(obj) = weak_obj.upgrade()
                    && let Some(color) = values
                        .get(1)
                        .and_then(|value| value.get::<gdk::RGBA>().ok())
                {
                    let imp = obj.imp();
                    imp.updating_color.set(true);
                    imp.draw_color.set_rgba(&color);
                    imp.updating_color.set(false);
                    imp.add_recent_color(color);
                }
                None
            });

            self.draw_toolbar.set_visible(false);
            self.draw_statusbar.set_visible(false);
            self.update_draw_buttons();
        }

        /// Put the current drawing (or the selection when one exists) on the
        /// system clipboard as a PNG image.
        pub fn copy_to_clipboard(&self) {
            let Some(png) = self.draw_canvas.copy_selection() else {
                return;
            };

            let bytes = glib::Bytes::from_owned(png);
            if let Ok(texture) = gdk::Texture::from_bytes(&bytes) {
                let provider_texture = gdk::ContentProvider::for_value(&texture.to_value());
                let provider_bytes = gdk::ContentProvider::for_bytes("image/png", &bytes);
                let provider = gdk::ContentProvider::new_union(&[provider_texture, provider_bytes]);
                let _ = self.obj().clipboard().set_content(Some(&provider));
                self.obj().window_show_toast(
                    &gettext("Image copied to clipboard"),
                    adw::ToastPriority::Normal,
                );
            }
        }

        /// Cut the selection to the clipboard, clearing it from the canvas.
        fn cut_to_clipboard(&self) {
            let Some(png) = self.draw_canvas.cut_selection() else {
                return;
            };

            let bytes = glib::Bytes::from_owned(png);
            if let Ok(texture) = gdk::Texture::from_bytes(&bytes) {
                let provider_texture = gdk::ContentProvider::for_value(&texture.to_value());
                let provider_bytes = gdk::ContentProvider::for_bytes("image/png", &bytes);
                let provider = gdk::ContentProvider::new_union(&[provider_texture, provider_bytes]);
                let _ = self.obj().clipboard().set_content(Some(&provider));
                self.obj().window_show_toast(
                    &gettext("Image cut to clipboard"),
                    adw::ToastPriority::Normal,
                );
            }
            self.update_draw_buttons();
        }

        /// Paste the clipboard texture into the canvas, scaled down to fit when
        /// it is larger than the image.
        fn paste_from_clipboard(&self) {
            let clipboard = self.obj().clipboard();
            let weak_obj = self.obj().downgrade();

            glib::spawn_future_local(async move {
                let Ok(texture) = clipboard.read_texture_future().await else {
                    return;
                };
                let Some(texture) = texture else {
                    return;
                };
                let Some(obj) = weak_obj.upgrade() else {
                    return;
                };

                let bytes = texture.save_to_png_bytes();
                if obj.imp().draw_canvas.paste_png(&bytes).is_ok() {
                    obj.imp().update_draw_buttons();
                }
            });
        }

        /// Run a filter off the main thread so the window stays responsive on
        /// large images, with the editor controls disabled meanwhile.
        fn apply_filter(&self, filter: DrawFilter) {
            if self.filter_busy.replace(true) {
                return;
            }

            self.update_draw_buttons();
            let weak_obj = self.obj().downgrade();
            self.draw_canvas.apply_filter_async(filter, move |applied| {
                if let Some(obj) = weak_obj.upgrade() {
                    obj.imp().filter_busy.set(false);
                    obj.imp().update_draw_buttons();
                    if applied {
                        obj.imp().update_save_state(false);
                    }
                }
            });
        }

        fn setup_filters(&self) {
            let canvas = &self.draw_canvas;

            for filter in canvas.available_filters() {
                let label = gtk::Label::builder()
                    .label(filter.label())
                    .xalign(0.)
                    .build();
                let button = gtk::Button::builder()
                    .child(&label)
                    .has_frame(false)
                    .build();
                let row = gtk::ListBoxRow::new();
                row.set_child(Some(&button));
                row.set_activatable(false);
                self.filter_list.append(&row);

                let weak_obj = self.obj().downgrade();
                button.connect_clicked(move |_| {
                    if let Some(obj) = weak_obj.upgrade() {
                        obj.imp().apply_filter(filter);
                        obj.imp().draw_filters.popdown();
                    }
                });
            }
        }

        /// The editor now always opens in drawing mode. Cropping is folded into
        /// the drawing canvas, so there is no mode switch left to drive.
        fn start_drawing(&self) {
            self.mode_stack.set_visible_child_name("draw");
            self.draw_toolbar.set_visible(true);
            self.draw_statusbar.set_visible(true);
            self.ensure_draw_loaded();
        }

        fn ensure_draw_loaded(&self) {
            if self.draw_canvas.has_image() || self.draw_loading.get() {
                return;
            }

            let obj = self.obj();
            let Some(file) = obj.original_image().file() else {
                return;
            };

            self.draw_loading.set(true);

            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                async move {
                    let imp = obj.imp();
                    if let Err(err) = imp.draw_canvas.load(file).await {
                        tracing::warn!("Failed to load image for drawing: {err}");
                        obj.window_show_error(
                            &gettext("Failed to open the image for drawing."),
                            &format!("Failed to load the image for drawing:\n\n{err}"),
                            ErrorType::Loader,
                        );
                    }
                    imp.draw_loading.set(false);
                }
            ));
        }

        fn update_draw_buttons(&self) {
            let has_image = self.draw_canvas.has_image();
            let busy = self.filter_busy.get();

            self.draw_undo.set_sensitive(!busy && self.draw_canvas.can_undo());
            self.draw_redo.set_sensitive(!busy && self.draw_canvas.can_redo());
            self.draw_copy.set_sensitive(has_image && !busy);
            self.draw_cut
                .set_sensitive(has_image && !busy && self.draw_canvas.has_selection());
            self.draw_paste.set_sensitive(has_image && !busy);
            self.draw_filters.set_sensitive(has_image && !busy);
            self.draw_clear.set_sensitive(has_image && !busy);
            self.draw_canvas.set_sensitive(!busy);
        }

        async fn save_draw(&self, new_file: &gio::File, cancellable: gio::Cancellable) -> bool {
            let obj = self.obj();

            let Some((data, width, height, stride)) = self.draw_canvas.surface_snapshot() else {
                return false;
            };

            let mime_type = obj
                .original_image()
                .file()
                .and_then(|file| file.path())
                .map(|path| gio::content_type_guess(Some(&path), None).0.to_string())
                .filter(|mime_type| mime_type == "image/jpeg")
                .unwrap_or_else(|| "image/png".to_string());

            self.set_saving_status(Some(gettext("Saving Image")));

            let mut creator = match glycin::Creator::new(glycin::MimeType::new(mime_type)).await {
                Ok(creator) => creator,
                Err(err) => {
                    tracing::warn!("Failed to create image encoder: {err}");
                    obj.window_show_error(
                        &gettext("Failed to save image."),
                        &format!("Failed to create image encoder:\n\n{err}"),
                        ErrorType::General,
                    );
                    self.set_saving_status(None);
                    return false;
                }
            };

            if let Err(err) = creator.add_frame_with_stride(
                width as u32,
                height as u32,
                stride as u32,
                glycin::MemoryFormat::B8g8r8a8Premultiplied,
                data,
            ) {
                tracing::warn!("Failed to add frame: {err}");
                obj.window_show_error(
                    &gettext("Failed to save image."),
                    &format!("Failed to add image frame:\n\n{err}"),
                    ErrorType::General,
                );
                self.set_saving_status(None);
                return false;
            }

            let encoded = match creator.create().await {
                Ok(encoded) => encoded,
                Err(err) => {
                    tracing::warn!("Failed to encode image: {err}");
                    obj.window_show_error(
                        &gettext("Failed to save image."),
                        &format!("Failed to encode image:\n\n{err}"),
                        ErrorType::General,
                    );
                    self.set_saving_status(None);
                    return false;
                }
            };

            let save_result = gio::CancellableFuture::new(
                new_file.replace_contents_future(
                    encoded.data_full(),
                    None,
                    true,
                    gio::FileCreateFlags::NONE,
                ),
                cancellable,
            )
            .await;

            if save_result.is_err() {
                tracing::debug!("Saving image canceled");
                self.set_saving_status(None);
                return false;
            }

            if let Ok(Err(err)) = save_result {
                obj.window_show_error(
                    &gettext("Failed to Save Image"),
                    &format!("Failed to write file:\n\n{err:?}"),
                    ErrorType::General,
                );
                self.set_saving_status(None);
                return false;
            }

            self.draw_canvas.mark_saved();
            self.set_saving_status(None);
            true
        }

        /// Close editing view
        fn cancel(&self) {
            let obj = self.obj().to_owned();

            if let Some(cancellable) = obj.imp().save_cancellable.replace(None) {
                cancellable.cancel();
            }

            glib::spawn_future_local(async move {
                if obj.ensure_saved().await == glib::Propagation::Proceed {
                    obj.window_inspect(|w| w.show_image());
                }
            });
        }

        async fn save_copy(&self) {
            let obj = self.obj();
            self.save.popdown();

            if let Some(current_file) = obj.original_image().file() {
                let cancellable = gio::Cancellable::new();
                if let Some(old_cancellable) =
                    self.save_cancellable.replace(Some(cancellable.clone()))
                {
                    old_cancellable.cancel();
                }

                let file_dialog = gtk::FileDialog::new();

                let suggested_file = if let Some(path) = current_file.path() {
                    if let Some(basename) = path.file_stem() {
                        let mut suggested_file = None;
                        for i in 1..20 {
                            let mut new_path = path.clone();
                            let mut new_filename = basename.to_os_string();

                            // Translators: Filename suffix, {} is replaced by a number
                            new_filename.push(ngettext(" (Edited)", " (Edited {})", i));

                            if let Some(ext) = path.extension() {
                                new_filename.push(".");
                                new_filename.push(ext);
                            }

                            new_path.set_file_name(new_filename);

                            let file = gio::File::for_path(&new_path);
                            let file_exists = file
                                .query_info_future(
                                    gio::FILE_ATTRIBUTE_STANDARD_NAME,
                                    gio::FileQueryInfoFlags::NONE,
                                    glib::Priority::DEFAULT,
                                )
                                .await
                                .is_ok();

                            if !file_exists {
                                suggested_file = Some(file);
                                break;
                            }
                        }

                        suggested_file.unwrap_or_else(|| current_file.clone())
                    } else {
                        current_file.clone()
                    }
                } else {
                    current_file.clone()
                };

                file_dialog.set_initial_file(Some(&suggested_file));

                match file_dialog.save_future(obj.try_window().as_ref()).await {
                    Err(err) => {
                        tracing::error!("{}", err);
                    }
                    Ok(new_file) => {
                        if self
                            .save(current_file, new_file.clone(), cancellable.clone())
                            .await
                        {
                            self.has_unsaved_changes.replace(false);
                            obj.window_inspect(|w| w.show_specific_image(new_file));
                        }
                    }
                }
            }
        }

        pub(super) async fn save_overwrite(&self) -> Result<(), ()> {
            let obj = self.obj();
            self.save.popdown();

            if let Some(original_file) = obj.original_image().file()
                && let Some(current_path) = original_file.path()
                && let Some(mut file_stem) = current_path.file_stem().map(|x| x.to_os_string())
            {
                let mut tmp_path = current_path.clone();
                file_stem.push(".tmp");
                if let Some(ext) = current_path.extension() {
                    file_stem.push(".");
                    file_stem.push(ext);
                }
                tmp_path.set_file_name(file_stem);

                let tmp_file = gio::File::for_path(&tmp_path);

                let cancellable = gio::Cancellable::new();
                if let Some(old_cancellable) =
                    self.save_cancellable.replace(Some(cancellable.clone()))
                {
                    old_cancellable.cancel();
                }

                tracing::debug!("Saving changes …");
                let written = self
                    .save(original_file.clone(), tmp_file.clone(), cancellable.clone())
                    .await;
                tracing::debug!("Image written: {written}");

                if written {
                    tracing::debug!("Moving image to trash '{}'", original_file.uri());
                    self.set_saving_status(Some(gettext("Moving Original to Trash")));
                    let trash_result = gio::CancellableFuture::new(
                        original_file.trash_future(glib::Priority::DEFAULT),
                        cancellable.clone(),
                    )
                    .await;

                    if trash_result.is_err() {
                        // Canceled
                        tracing::debug!("Trashing image canceled");
                        self.set_saving_status(None);
                        return Err(());
                    } else if let Ok(Err(err)) = trash_result {
                        obj.window_show_error(
                                    &gettext("Failed to save image."),
                                    &format!("Failed to move image {current_path:?} to trash and therefore couldn't save image: {err}"),
                                    ErrorType::General,
                                );
                        self.set_saving_status(None);
                        return Err(());
                    }

                    tracing::debug!("Moving '{}' to '{}'", tmp_file.uri(), original_file.uri());
                    self.set_saving_status(Some(gettext("Finalizing Save Operation")));
                    let move_result = gio::CancellableFuture::new(
                        tmp_file
                            .move_future(
                                &original_file,
                                gio::FileCopyFlags::NONE,
                                glib::Priority::DEFAULT,
                            )
                            .0,
                        cancellable.clone(),
                    )
                    .await;

                    if move_result.is_err() {
                        // Canceled
                        tracing::debug!("Moving image canceled");
                        self.set_saving_status(None);
                        return Err(());
                    } else if let Ok(Err(err)) = move_result {
                        obj.window_show_error(
                                        &gettext("Failed to save image."),
                                        &format!(
                                            "Failed to move image to {current_path:?}: {err}; moving {:?} to {:?}",
                                            tmp_file.path().unwrap_or_default(),
                                            original_file.path().unwrap_or_default(),
                                        ),
                                        ErrorType::General,
                                    );
                        self.set_saving_status(None);
                        return Err(());
                    }

                    self.has_unsaved_changes.replace(false);
                    obj.window_inspect(|w| w.show_specific_image(original_file));
                }
            }

            Ok(())
        }

        #[must_use]
        /// Saves image with operations applies
        ///
        /// Returns `true` if editing and saving was successful
        async fn save(
            &self,
            current_file: gio::File,
            new_file: gio::File,
            cancellable: gio::Cancellable,
        ) -> bool {
            let obj = self.obj();

            if self.draw_canvas.has_changes() {
                tracing::debug!("Saving drawing");
                return self.save_draw(&new_file, cancellable).await;
            }

            
            self.set_saving_status(Some(gettext("Editing Image")));
            let mut editor = glycin::Editor::new(current_file);
            editor.cancellable(cancellable.clone());

            if let Some(operations) = obj.operations() {
                tracing::debug!("Computing edited image.");
                let result = match editor.edit().await {
                    Ok(editable_image) => editable_image.apply_complete(&operations).await,
                    Err(err) => Err(err),
                };
                match result {
                    Err(err) if err.is_cancelled() => {
                        tracing::debug!("Computing edited image canceled");
                    }
                    Err(err) => {
                        tracing::warn!("Failed to edit image: {err}");
                        obj.window_show_error(
                            &gettext("Failed to edit image."),
                            &format!("Failed to edit image:\n\n{err}\n\n{operations:#?}"),
                            ErrorType::Loader,
                        )
                    }
                    Ok(edit) => {
                        tracing::debug!("Saving edited image to '{}'", new_file.uri());
                        self.set_saving_status(Some(gettext("Saving Image")));

                        // `replace_contents_future` needs an owned, `Send` buffer and
                        // glycin only lends us the bytes, so this copy is required.
                        let data = edit.data().to_vec();
                        let save_result = gio::CancellableFuture::new(
                            new_file.replace_contents_future(
                                data,
                                None,
                                true,
                                gio::FileCreateFlags::NONE,
                            ),
                            cancellable,
                        )
                        .await;

                        if save_result.is_err() {
                            tracing::debug!("Saving image canceled");
                        } else if let Ok(Err(err)) = save_result {
                            obj.window_show_error(
                                &gettext("Failed to Save Image"),
                                &format!("Failed to write file:\n\n{err:?}"),
                                ErrorType::General,
                            );
                        } else {
                            tracing::debug!("Image saved");
                            return true;
                        }
                    }
                }
            }

            self.set_saving_status(None);
            false
        }

        fn set_saving_status(&self, message: Option<String>) {
            let is_saving = message.is_some();
            self.saving_status.set_label(&message.unwrap_or_default());
            self.saving_revealer.set_reveal_child(is_saving);
            self.update_save_state(is_saving);

            if is_saving {
                // Show text and spinner delayed
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(
                        self.saving_revealer.transition_duration().into(),
                    ),
                    glib::clone!(
                        #[weak(rename_to=imp)]
                        self,
                        move || {
                            imp.saving_info.set_reveal_child(true);
                        }
                    ),
                );
            } else {
                self.saving_info.set_reveal_child(false);
            }
        }

        pub(super) fn is_save_sensitive(&self) -> bool {
            self.operations
                .borrow()
                .as_ref()
                .is_some_and(|x| !x.operations().is_empty())
                || self.draw_canvas.has_changes()
        }

        pub fn update_save_state(&self, force_disabled: bool) {
            let obj = self.obj();

            if !force_disabled {
                self.has_unsaved_changes.replace(self.is_save_sensitive());
            }

            let enabled = self.is_save_sensitive() && !force_disabled;

            self.save.set_sensitive(enabled);
            obj.action_set_enabled("edit.save-overwrite", enabled & self.can_trash.get());
            obj.action_set_enabled("edit.save-copy", enabled);
        }
    }
}

glib::wrapper! {
    pub struct LpEditWindow(ObjectSubclass<imp::LpEditWindow>)
    @extends gtk::Widget, adw::Bin,
    @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl LpEditWindow {
    pub const REQUIRED_OPERATIONS: &[glycin::OperationId] = &[
        glycin::OperationId::Clip,
        glycin::OperationId::MirrorHorizontally,
        glycin::OperationId::Rotate,
    ];

    pub fn new(image: LpImage) -> Self {
        glib::Object::builder()
            .property("original_image", image)
            .build()
    }

    pub fn operations(&self) -> Option<Arc<glycin::Operations>> {
        self.imp().operations.borrow().clone()
    }

    pub fn set_operations(&self, operations: Option<Arc<glycin::Operations>>) {
        let imp = self.imp();
        imp.operations.replace(operations);
        imp.update_save_state(false);
    }

    pub fn add_operation(&self, operation: glycin::Operation) {
        let imp = self.imp();

        let mut operations = imp
            .operations
            .borrow()
            .as_ref()
            .map(|x| x.operations().to_vec())
            .unwrap_or_default();

        operations.push(operation);

        self.set_operations(Some(Arc::new(Operations::new(operations))));
    }

    pub fn copy_to_clipboard(&self) {
        self.imp().copy_to_clipboard();
    }

    /// Returns if the window should proceed with closing
    pub async fn ensure_saved(&self) -> glib::Propagation {
        if self.has_unsaved_changes() {
            let dialog = adw::AlertDialog::new(
                Some(&gettext("Save Changes?")),
                Some(&gettext(
                    "There are unsaved image editing changes. Closing without saving will permanently lose these modifications.",
                )),
            );

            dialog.add_response("cancel", &gettext("Cancel"));
            dialog.add_response("discard", &gettext("Discard"));
            dialog.add_response("save", &gettext("Save"));

            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);

            let res = dialog.choose_future(Some(self)).await;

            match res.as_str() {
                "cancel" => glib::Propagation::Stop,
                "close" => glib::Propagation::Stop,
                "discard" => {
                    self.imp().has_unsaved_changes.replace(false);
                    glib::Propagation::Proceed
                }
                "save" => {
                    if self.imp().save_overwrite().await.is_ok() {
                        glib::Propagation::Proceed
                    } else {
                        glib::Propagation::Stop
                    }
                }
                res => {
                    tracing::error!("Invalid dialog result: {res}");
                    glib::Propagation::Stop
                }
            }
        } else {
            glib::Propagation::Proceed
        }
    }
}
