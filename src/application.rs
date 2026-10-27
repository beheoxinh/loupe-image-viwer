// Copyright (c) 2020-2023 Christopher Davis
// Copyright (c) 2022-2026 Sophie Herold
// Copyright (c) 2023 Matteo Nardi
// Copyright (c) 2023 Julian Hofer
// Copyright (c) 2023 Huan Nguyen
// Copyright (c) 2024 Balló György
// Copyright (c) 2025 Abhinandan Thakur
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

use adw::prelude::*;
use adw::subclass::prelude::*;

use crate::config;
use crate::deps::*;
use crate::util::gettext::*;
use crate::widgets::LpWindow;

mod imp {
    use super::*;
    use crate::widgets::LpShyBin;

    // The basic struct that holds our
    // state and widgets
    #[derive(Debug)]
    pub struct LpApplication {
        pub settings: gio::Settings,
    }

    impl Default for LpApplication {
        fn default() -> Self {
            Self {
                settings: gio::Settings::new(config::APP_ID),
            }
        }
    }

    // Sets up the basics for the GObject
    // The `#[glib::object_subclass] macro implements
    // some boilerplate code for the object setup, e.g. get_type()
    #[glib::object_subclass]
    impl ObjectSubclass for LpApplication {
        const NAME: &'static str = "LpApplication";
        type Type = super::LpApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for LpApplication {
        fn constructed(&self) {
            let obj = self.obj();

            self.parent_constructed();

            // Set up the actions
            obj.setup_actions();
        }
    }

    impl ApplicationImpl for LpApplication {
        fn startup(&self) {
            tracing::trace!("Startup");
            self.parent_startup();

            // Force dark theme
            self.obj()
                .style_manager()
                .set_color_scheme(adw::ColorScheme::PreferDark);

            // Load the editor stylesheet above the user's GTK theme.
            //
            // GTK loads `$XDG_CONFIG_HOME/gtk-4.0/gtk.css` — which is how most
            // themes ship — at `STYLE_PROVIDER_PRIORITY_USER`. The default
            // `STYLE_PROVIDER_PRIORITY_APPLICATION` is *below* that, so any rule
            // a theme sets (button padding, minimum sizes) silently wins over
            // ours. The editor's square tool buttons depend on our rules
            // winning, so `edit.css` is re-registered one step above the user
            // priority. Only `edit.css` is re-registered, not the whole
            // stylesheet, so the rest of the app keeps the normal cascade.
            let provider = gtk::CssProvider::new();
            provider.load_from_resource("/org/gnome/Loupe/edit.css");
            gtk::style_context_add_provider_for_display(
                &gdk::Display::default().expect("A display is required to start a GTK application"),
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
            );

            LpShyBin::ensure_type();
            gtk::Window::set_default_icon_name(config::APP_ID);
        }

        fn activate(&self) {
            tracing::debug!("Showing window via 'activate'");
            let application = self.obj();
            let window = LpWindow::new(&*application);
            window.present();
        }

        // Handles opening files from the command line or other applications
        fn open(&self, files: &[gio::File], _hint: &str) {
            tracing::debug!("Open {} file(s)", files.len());
            let application = self.obj();
            let win = LpWindow::new(&*application);
            win.image_view().set_images_from_files(files.to_vec());
        }
    }

    // This is empty, but we still need to provide an
    // empty implementation for each type we subclass.
    impl GtkApplicationImpl for LpApplication {}
    impl AdwApplicationImpl for LpApplication {}
}

// Creates a wrapper struct that inherits the functions
// from objects listed as @extends or interfaces it @implements.
// This is what allows us to do e.g. application.quit() on
// LpApplication without casting.
glib::wrapper! {
    pub struct LpApplication(ObjectSubclass<imp::LpApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

// This is where the member functions of LpApplication go.
#[allow(clippy::new_without_default)]
impl LpApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", config::APP_ID)
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .property("resource-base-path", "/org/gnome/Loupe")
            .build()
    }

    pub fn setup_actions(&self) {
        // gio::ActionEntryBuilder allows us to build and store an action on an object
        // that implements gio::ActionMap. Here we build the application's actions and
        // add them with add_action_entries().
        let actions = [
            gio::ActionEntryBuilder::new("help")
                .activate(|app: &Self, _, _| app.show_help())
                .build(),
            gio::ActionEntryBuilder::new("preferences")
                .activate(|app: &Self, _, _| app.show_preferences())
                .build(),
            gio::ActionEntryBuilder::new("quit")
                .activate(|app: &Self, _, _| {
                    // Give all windows the chance to handle close-request
                    for win in app.windows() {
                        win.close();
                    }
                })
                .build(),
            gio::ActionEntryBuilder::new("new-window")
                .activate(|app: &Self, _, _| {
                    let win = LpWindow::new(app);
                    tracing::debug!("Showing new window");
                    win.present();
                })
                .build(),
        ];

        self.add_action_entries(actions);

        self.set_accels_for_action("app.help", &["F1"]);
        self.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);
        self.set_accels_for_action("app.quit", &["<Ctrl>Q"]);
        self.set_accels_for_action("window.close", &["<Ctrl>W"]);
        self.set_accels_for_action("app.new-window", &["<Ctrl>N"]);
    }

    /// Show the application preferences, including the external editor choice.
    pub fn show_preferences(&self) {
        let settings = self.settings();

        let names = gtk::StringList::new(&[]);
        names.append(&gettext("Ask every time"));

        // The index in `app_ids` matches the index in the `names` string list.
        let mut app_ids = vec![String::new()];

        // Collect every application that can act as an image editor. Relying only
        // on `AppInfo::all_for_type` misses editors that register only a few (or
        // hidden per-format) desktop entries, and apps that take `%U` instead of
        // `%f` (e.g. KDE apps).
        let graphics_categories = [
            "graphics",
            "2dgraphics",
            "rastergraphics",
            "vectorgraphics",
            "photography",
            "drawing",
        ];
        let excluded_categories = [
            "webbrowser",
            "texteditor",
            "development",
            "ide",
            "spreadsheet",
            "wordprocessor",
            "presentation",
            "audiovideo",
            "database",
            "consoleonly",
            "viewer",
        ];
        let editor_types = [
            "application/x-krita",
            "image/x-kra",
            "image/x-xcf",
            "image/x-psd",
            "application/x-xcf",
        ];

        // id, display name, whether it is shown in app grids, image type count
        let mut candidates: Vec<(String, String, bool, usize)> = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for app in gio::AppInfo::all() {
            let Some(id) = app.id() else {
                continue;
            };
            if id.as_str() == config::APP_ID || !seen_ids.insert(id.to_string()) {
                continue;
            }
            if !(app.supports_files() || app.supports_uris()) {
                continue;
            }

            let categories = gio_unix::DesktopAppInfo::new(id.as_str())
                .and_then(|info| info.categories())
                .map(|categories| categories.to_lowercase())
                .unwrap_or_default();
            let categories: Vec<&str> = categories
                .split(';')
                .filter(|category| !category.is_empty())
                .collect();

            if categories
                .iter()
                .any(|category| excluded_categories.contains(category))
            {
                continue;
            }

            let supported_types = app.supported_types();
            let image_type_count = supported_types
                .iter()
                .filter(|content_type| content_type.starts_with("image/"))
                .count();
            let has_editor_type = supported_types
                .iter()
                .any(|content_type| editor_types.contains(&content_type.as_str()));
            let is_graphics_app = categories
                .iter()
                .any(|category| graphics_categories.contains(category));

            // Apps without categories are only accepted when they support a
            // format that is specific to editing.
            if !is_graphics_app && !(categories.is_empty() && has_editor_type) {
                continue;
            }

            candidates.push((
                id.to_string(),
                app.display_name().to_string(),
                app.should_show(),
                image_type_count,
            ));
        }

        // Many apps register several per-format desktop entries (e.g. Krita).
        // Keep one entry per display name, preferring the visible entry and the
        // one that advertises the most image types.
        let mut best_by_name: std::collections::HashMap<String, (bool, usize, String, String)> =
            std::collections::HashMap::new();
        for (id, name, show, image_type_count) in candidates {
            let key = name.to_lowercase();
            let is_better = best_by_name
                .get(&key)
                .is_none_or(|(show_best, count_best, _, _)| {
                    (show, image_type_count) > (*show_best, *count_best)
                });
            if is_better {
                best_by_name.insert(key, (show, image_type_count, id, name));
            }
        }

        let mut apps: Vec<(String, String)> = best_by_name
            .into_values()
            .map(|(_, _, id, name)| (name, id))
            .collect();
        apps.sort_by_key(|(name, _)| name.to_lowercase());

        for (name, id) in apps {
            names.append(&name);
            app_ids.push(id);
        }

        let current = settings.string("external-editor");
        let selected = app_ids
            .iter()
            .position(|id| id.as_str() == current.as_str())
            .unwrap_or(0) as u32;

        let row = adw::ComboRow::new();
        row.set_title(&gettext("Image Editor"));
        row.set_subtitle(&gettext("Used by the Edit in External App action"));
        row.set_model(Some(&names));
        row.set_selected(selected);
        row.connect_selected_notify(glib::clone!(
            #[strong]
            settings,
            #[strong]
            app_ids,
            move |row| {
                let Some(id) = app_ids.get(row.selected() as usize) else {
                    return;
                };

                if let Err(err) = settings.set_string("external-editor", id) {
                    tracing::warn!("Failed to save external-editor setting: {}", err);
                }
            }
        ));

        let confirm_row = adw::SwitchRow::new();
        confirm_row.set_title(&gettext("Confirm Before Opening"));
        confirm_row.set_subtitle(&gettext(
            "Ask for confirmation before launching the external editor and closing the viewer",
        ));
        confirm_row.set_active(settings.boolean("external-editor-confirm"));
        confirm_row.connect_active_notify(glib::clone!(
            #[strong]
            settings,
            move |row| {
                if let Err(err) = settings.set_boolean("external-editor-confirm", row.is_active()) {
                    tracing::warn!("Failed to save external-editor-confirm setting: {}", err);
                }
            }
        ));

        let group = adw::PreferencesGroup::new();
        group.set_title(&gettext("External Editor"));
        group.add(&row);
        group.add(&confirm_row);

        let page = adw::PreferencesPage::new();
        page.add(&group);

        let dialog = adw::PreferencesDialog::new();
        dialog.set_title(&gettext("Preferences"));
        dialog.add(&page);
        dialog.present(self.active_window().as_ref());
    }

    pub fn show_help(&self) {
        let context = self
            .active_window()
            .map(|w| gtk::prelude::WidgetExt::display(&w).app_launch_context());
        glib::spawn_future_local(async move {
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri_future("help:loupe", context.as_ref()).await
            {
                tracing::error!("Failed to launch help: {}", e.message());
            }
        });
    }

    pub fn settings(&self) -> gio::Settings {
        self.imp().settings.clone()
    }
}

impl Default for LpApplication {
    fn default() -> Self {
        gio::Application::default()
            .unwrap()
            .downcast::<LpApplication>()
            .unwrap()
    }
}
