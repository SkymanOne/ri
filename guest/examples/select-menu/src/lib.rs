//! `/menu` opens a drinks menu: a select list between two borders, which
//! Enter picks from and Escape closes, then reports the choice.
//!
//! A port of the TypeScript extension that yapi's `ext-ui-custom-*`
//! scenarios use, built on the SDK's `widgets`.

use yapi_extension_api::widgets::tui::lines::{border, text_row};
use yapi_extension_api::widgets::tui::select_list::{
    SelectEvent, SelectItem, SelectList, SelectListLayout, SelectListTheme,
};
use yapi_extension_api::widgets::{keybindings, parse, style, to_ansi};
use yapi_extension_api::{Api, Component, CustomOptions, Done, notify, theme};

struct Menu {
    list: SelectList,
    done: Done<Option<String>>,
}

impl Component for Menu {
    fn render(&mut self, width: usize) -> Vec<String> {
        let th = theme();
        let title = parse(&th.fg("accent", &th.bold("Drinks")));
        let mut rows = vec![border(width, style("accent"))];
        rows.extend(text_row(title, width, 1));
        rows.extend(self.list.render(width));
        rows.push(border(width, style("accent")));
        to_ansi(&rows, None)
    }

    fn handle_input(&mut self, data: &str) {
        match self.list.handle_input(data, &keybindings()) {
            SelectEvent::Selected(item) => self.done.finish(Some(item.value)),
            SelectEvent::Cancelled => self.done.finish(None),
            SelectEvent::Moved | SelectEvent::Ignored => {}
        }
    }
}

fn init(api: &mut Api) {
    api.register_command("menu", "Open a drinks menu", |_args, ctx| async move {
        let item = |value: &str, label: &str, description: Option<&str>| SelectItem {
            value: value.into(),
            label: label.into(),
            description: description.map(Into::into),
        };
        let items = vec![
            item("tea", "Tea", Some("Hot")),
            item("juice", "Juice", Some("Cold")),
            item("water", "Water", None),
        ];
        let list = SelectList::new(
            items,
            5,
            SelectListTheme::from_theme(style),
            SelectListLayout::default(),
        );
        let choice = ctx
            .custom(|done| Menu { list, done }, CustomOptions::default())
            .await
            .flatten();
        let message = match choice {
            Some(choice) => format!("Chose {choice}"),
            None => "Menu closed".to_owned(),
        };
        notify(&message, "info");
        Ok(())
    });
}

yapi_extension_api::extension!(init);
