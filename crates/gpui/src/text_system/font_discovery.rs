use crate::{App, Global, PlatformTextSystem};

/// A discovered font-family snapshot, without GPUI's synthetic fallback aliases.
/// An empty snapshot can also represent discovery in progress; consult
/// [`SystemFontsLoading`] to distinguish it from a completed empty catalog.
pub struct SystemFontFamilies(pub Vec<String>);

impl Global for SystemFontFamilies {}

/// Completion signal for background platform-font discovery.
/// The channel closes after discovery; it never sends individual messages.
pub struct SystemFontsLoading(pub async_channel::Receiver<()>);

impl Global for SystemFontsLoading {}

impl App {
    /// Discover and register platform fonts on the background executor once.
    ///
    /// Installs an empty [`SystemFontFamilies`] snapshot and a
    /// [`SystemFontsLoading`] signal before returning. The platform loader runs
    /// after the current foreground callback yields, so window creation need
    /// not wait for font discovery. Completion publishes the actual families,
    /// releases every waiter and refreshes windows on the foreground thread.
    pub fn load_system_fonts(
        &mut self,
        load: impl FnOnce(&dyn PlatformTextSystem) + Send + 'static,
    ) {
        if self.has_global::<SystemFontsLoading>() {
            return;
        }
        let (ready, receiver) = async_channel::bounded::<()>(1);
        self.set_global(SystemFontsLoading(receiver));
        self.set_global(SystemFontFamilies(Vec::new()));
        let text_system = self.text_system().clone();
        let executor = self.background_executor().clone();
        self.spawn(async move |cx| {
            let families = executor
                .spawn(async move {
                    load(text_system.platform_text_system.as_ref());
                    text_system.font_family_names()
                })
                .await;
            cx.wait_for_update_slot().await;
            cx.update(move |cx| {
                // Observers must see completion even when no fonts were found.
                drop(ready);
                cx.set_global(SystemFontFamilies(families));
                cx.refresh_windows();
            });
        })
        .detach();
    }
}
