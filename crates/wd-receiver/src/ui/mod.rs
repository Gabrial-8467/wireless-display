pub mod connection_page;
pub mod devices_page;
pub mod diagnostics_page;
pub mod window;

pub(crate) fn set_uniform_margins<W: gtk::prelude::IsA<gtk::Widget>>(widget: &W, margin: i32) {
    use gtk::prelude::*;
    widget.set_margin_top(margin);
    widget.set_margin_bottom(margin);
    widget.set_margin_start(margin);
    widget.set_margin_end(margin);
}
