//! Loading a document without a flash: the pending load, and the page
//! fade that covers the swap.

use super::MessageView;

use adw::prelude::*;
use relm4::prelude::*;
use webkit6::prelude::WebViewExt;

/// A document waiting for the WebView to be ready for it (`load_after_paint`).
#[derive(Default)]
pub(super) struct PendingLoad {
    /// The newest document, its base URI, and whether the page on screen
    /// fades into it. Only the newest is kept.
    doc: Option<(String, String, bool)>,
    /// Bumped per wait, so a superseded wait's fallback cannot load the next
    /// document before that one's own wait is over.
    wait: u64,
    /// The WebView's size when the last document was loaded.
    size: (i32, i32),
}

thread_local! {
    /// Run as the reader's next page change shows (`on_next_page_fade`).
    static ON_PAGE_FADE: std::cell::RefCell<Vec<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `f` when the reader's next page change starts to show, so something
/// leaving with the old message (the attachment drawer) dissolves with it
/// rather than ahead of it.
pub(crate) fn on_next_page_fade(f: impl FnOnce() + 'static) {
    ON_PAGE_FADE.with(|waiting| waiting.borrow_mut().push(Box::new(f)));
}

fn page_fade_started() {
    for f in ON_PAGE_FADE.with(|waiting| std::mem::take(&mut *waiting.borrow_mut())) {
        f();
    }
}

/// How long the outgoing page takes to fade out over the incoming one. The
/// attachment drawer comes and goes on the same beat.
pub(crate) const PAGE_FADE_MS: u32 = 80;

/// A still of the outgoing page, laid over the WebView while the next page
/// loads and faded out once that page has painted: one message dissolves
/// into the next instead of cutting to it.
#[derive(Clone)]
pub(super) struct PageFade {
    pub(super) cover: gtk::Picture,
    anim: std::rc::Rc<std::cell::RefCell<Option<adw::TimedAnimation>>>,
    /// Bumped per still, so a fade meant for one load can't reveal the
    /// next before that one has painted.
    generation: std::rc::Rc<std::cell::Cell<u64>>,
    /// A still is up and its page hasn't painted yet.
    held: std::rc::Rc<std::cell::Cell<bool>>,
}

impl PageFade {
    pub(super) fn new() -> Self {
        let cover = gtk::Picture::new();
        cover.set_content_fit(gtk::ContentFit::Fill);
        cover.set_can_shrink(true);
        cover.set_can_target(false);
        cover.set_can_focus(false);
        cover.set_visible(false);
        Self {
            cover,
            anim: Default::default(),
            generation: Default::default(),
            held: Default::default(),
        }
    }

    /// Cover the WebView with a still of what it shows now, then run
    /// `then` (the load that replaces it). The still is WebKit's own
    /// snapshot of the page: GTK's paintable of the widget draws nothing
    /// for a WebView.
    fn hold(&self, view: &webkit6::WebView, then: impl FnOnce() + 'static) {
        // Still waiting on the last page: the still already up is the page
        // the reader last saw, and the WebView under it may be half drawn.
        if self.held.get() || !view.is_mapped() {
            then();
            return;
        }
        if let Some(running) = self.anim.borrow_mut().take() {
            running.skip();
        }
        self.held.set(true);
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let then: SharedCallback = std::rc::Rc::new(std::cell::RefCell::new(Some(Box::new(then))));
        {
            let fade = self.clone();
            let then = then.clone();
            view.snapshot(
                webkit6::SnapshotRegion::Visible,
                webkit6::SnapshotOptions::NONE,
                None::<&gtk::gio::Cancellable>,
                move |still| {
                    // Too late: the load went ahead without a still, and
                    // this one would cover the page that replaced it.
                    let Some(then) = then.borrow_mut().take() else {
                        return;
                    };
                    if let Ok(still) = still {
                        if fade.held.get() && fade.generation.get() == generation {
                            fade.cover.set_paintable(Some(&still));
                            fade.cover.set_opacity(1.0);
                            fade.cover.set_visible(true);
                        }
                    }
                    then();
                },
            );
        }
        // A snapshot must not hold up the message: past a few frames the
        // load goes ahead, and the change is a plain cut.
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
            if let Some(then) = then.borrow_mut().take() {
                then();
            }
        });
        // A page that never reports in must not leave the still up.
        let fade = self.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_secs(1), move || {
            if fade.generation.get() == generation
                && fade.anim.borrow().is_none()
                && fade.cover.is_visible()
            {
                fade.held.set(false);
                fade.fade_out();
            }
        });
    }

    /// The next page has loaded: fade the still once it has been drawn
    /// (two animation frames in the page), not merely parsed.
    pub(super) fn release(&self, view: &webkit6::WebView) {
        // No still went up (a snapshot that failed or came too late, or a
        // page behind the spinner): the new page simply is there now.
        if !self.held.replace(false) || !self.cover.is_visible() {
            page_fade_started();
            return;
        }
        let generation = self.generation.get();
        let fade = self.clone();
        view.call_async_javascript_function(
            "await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));",
            None,
            None,
            None,
            None::<&gtk::gio::Cancellable>,
            move |_| {
                if fade.generation.get() == generation {
                    fade.fade_out();
                }
            },
        );
    }

    fn fade_out(&self) {
        page_fade_started();
        let cover = self.cover.clone();
        let target = adw::CallbackAnimationTarget::new(move |v| cover.set_opacity(v));
        let anim = adw::TimedAnimation::new(&self.cover, 1.0, 0.0, PAGE_FADE_MS, target);
        // A handful of frames at 60 Hz: an eased curve spends most of its
        // change in the first, which reads as a cut. Even steps dissolve.
        anim.set_easing(adw::Easing::Linear);
        let cover = self.cover.clone();
        anim.connect_done(move |_| {
            cover.set_visible(false);
            cover.set_paintable(None::<&gtk::gdk::Paintable>);
        });
        anim.play();
        if let Some(old) = self.anim.borrow_mut().replace(anim) {
            old.pause();
        }
    }
}

impl MessageView {
    /// Load a document without the WebView flashing black on the way.
    ///
    /// Opening a message often changes the WebView's size in the same turn:
    /// the attachment drawer comes or goes, the subject wraps differently.
    /// With WebKit's GPU (DMA-BUF) renderer, a page that commits before the
    /// web process has drawn at the new size has nothing to show at it, and
    /// the pane is black for a frame or two: the flash between messages
    /// that only real hardware showed. So the load waits for the frame that
    /// lays out the new size, and, when the size did change, for the old
    /// page to be drawn at it (two animation frames in the page), then
    /// replaces a page that is fully drawn. An unchanged size costs a frame.
    pub(super) fn load_after_paint(&self, html: String, uri: String, fade: bool) {
        let state = self.pending_load.clone();
        let wait = {
            let mut s = state.borrow_mut();
            // A wait already under way loads the newest document.
            if s.doc.replace((html, uri, fade)).is_some() {
                return;
            }
            s.wait = s.wait.wrapping_add(1);
            s.wait
        };
        let load = std::rc::Rc::new({
            let view = self.webview.clone();
            let page_fade = self.page_fade.clone();
            move || {
                let mut s = state.borrow_mut();
                // A later wait has taken over: this one's document is loaded.
                if s.wait != wait {
                    return;
                }
                if let Some((html, uri, fade)) = s.doc.take() {
                    s.size = (view.width(), view.height());
                    drop(s);
                    if fade {
                        let view = view.clone();
                        page_fade.hold(&view.clone(), move || {
                            crate::desktop::web_fonts::load_html(&view, &html, Some(&uri))
                        });
                    } else {
                        crate::desktop::web_fonts::load_html(&view, &html, Some(&uri));
                    }
                }
            }
        });
        let clock = self
            .webview
            .frame_clock()
            .filter(|_| self.webview.is_mapped());
        let Some(clock) = clock else {
            // Nothing on screen, so no frame to wait for or to flash.
            load();
            return;
        };
        let handler = std::rc::Rc::new(std::cell::Cell::new(None));
        let id = {
            let handler = handler.clone();
            let load = load.clone();
            let view = self.webview.clone();
            let size = self.pending_load.clone();
            clock.connect_after_paint(move |clock| {
                if let Some(id) = handler.take() {
                    clock.disconnect(id);
                }
                if size.borrow().size == (view.width(), view.height()) {
                    load();
                    return;
                }
                let load = load.clone();
                view.call_async_javascript_function(
                    "await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));",
                    None,
                    None,
                    None,
                    None::<&gtk::gio::Cancellable>,
                    move |_| load(),
                );
            })
        };
        handler.set(Some(id));
        clock.request_phase(gtk::gdk::FrameClockPhase::AFTER_PAINT);
        // A window that stops drawing (hidden, minimised) stops its clock and
        // the page's animation frames; the message must load all the same.
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            if let Some(id) = handler.take() {
                clock.disconnect(id);
            }
            load();
        });
    }
}

impl MessageView {
    /// While a document loads, keep the keyboard on the loading page rather
    /// than in the web view, and give it back once the document is shown.
    pub(super) fn hold_focus_while_loading(&self) {
        let Some(page) = &self.loading_page else {
            return;
        };
        let web: &gtk::Widget = self.webview.upcast_ref();
        let focus = web.root().and_then(|r| r.focus());
        if self.body_page() == "loading" {
            if focus.is_some_and(|f| f == *web || f.is_ancestor(web)) {
                page.grab_focus();
                self.focus_held.set(true);
            }
        } else if self.focus_held.replace(false)
            && focus.is_some_and(|f| f == *page.upcast_ref::<gtk::Widget>())
        {
            self.webview.grab_focus();
        }
    }

    /// Which body-stack page to show: spinner while fetching, themed cover while
    /// the WebView loads, then the rendered message(s).
    pub(super) fn body_page(&self) -> &'static str {
        // A document that has been handed over but hasn't painted yet is still
        // loading as far as the reader is concerned. Showing the previous
        // message in the meantime, or a bare cover between two spinners, is the
        // flash this avoids.
        if self.loading || !self.webview_ready {
            "loading"
        } else {
            "body"
        }
    }
}

/// A callback that runs once, from whichever of two paths gets there first.
type SharedCallback = std::rc::Rc<std::cell::RefCell<Option<Box<dyn FnOnce()>>>>;
