//! UserNotifications is deliberately disabled for command-line/unbundled runs:
//! currentNotificationCenter may raise an Objective-C exception without a bundle.
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, ensure};
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSBundle, NSError, NSObject, NSObjectProtocol, NSString, NSURL, NSUUID,
};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAttachment,
    UNNotificationDefaultActionIdentifier, UNNotificationPresentationOptions,
    UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter,
    UNUserNotificationCenterDelegate,
};

use super::shell::on_main;
use crate::platform::{Event, EventSink, Notifier};
use crate::raster::{Compression, Image};

// Only scalar handles leave the main thread. The weak native delegate is kept
// alive in NOTIFICATIONS until this handle is dropped and cleanup is dispatched.
pub struct Notifications {
    id: u64,
    enabled: bool,
    session: String,
}
struct DelegateState {
    shot_prefix: String,
    events: EventSink,
}
struct NativeNotifications {
    id: u64,
    center: Retained<UNUserNotificationCenter>,
    _delegate: Retained<NotificationDelegate>,
    authorized: Option<bool>,
    pending: VecDeque<Notice>,
}
struct Notice {
    identifier: String,
    summary: String,
    body: String,
    thumbnail: Option<Vec<u8>>,
}
thread_local! {
    static NOTIFICATIONS: RefCell<Option<NativeNotifications>> = const { RefCell::new(None) };
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_NOTICE: AtomicU64 = AtomicU64::new(1);

define_class!(
    #[unsafe(super = NSObject)]
    #[ivars = DelegateState]
    struct NotificationDelegate;
    unsafe impl NSObjectProtocol for NotificationDelegate {}
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            // Native delegate callbacks may arrive on a private queue. Only the
            // event delivery is marshalled to the Cocoa/GPUI main runloop.
            if &*response.actionIdentifier() == unsafe { UNNotificationDefaultActionIdentifier } {
                let identifier = response.notification().request().identifier().to_string();
                if let Some(shot) = identifier
                    .strip_prefix(&self.ivars().shot_prefix)
                    .and_then(|s| s.split('-').next())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    let events = self.ivars().events.clone();
                    on_main(move || {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            events(Event::OpenShot(shot))
                        }))
                        .is_err()
                        {
                            log::error!("macOS notification event callback panicked");
                        }
                    });
                }
            }
            completion.call(());
        }
    }
);

impl Notifications {
    pub fn new(events: EventSink) -> Result<Self> {
        ensure!(
            MainThreadMarker::new().is_some(),
            "notifications must be initialized on the main thread"
        );
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        // Old notifications must never refer to a new daemon's recycled shot IDs.
        let session = NSUUID::UUID().UUIDString().to_string();
        let bundle = NSBundle::mainBundle();
        let bundled = bundle
            .bundleIdentifier()
            .is_some_and(|s| s.to_string() == "io.github.qt715.quikscope")
            && bundle.bundlePath().to_string().ends_with(".app");
        if !bundled {
            log::warn!("macOS notifications unavailable outside the quikscope application bundle");
            return Ok(Self {
                id,
                enabled: false,
                session,
            });
        }
        ensure!(
            NOTIFICATIONS.with(|s| s.borrow().is_none()),
            "macOS notifications already initialized"
        );
        let this = NotificationDelegate::alloc().set_ivars(DelegateState {
            shot_prefix: format!("quikscope-{session}-shot-"),
            events,
        });
        let delegate: Retained<NotificationDelegate> = unsafe { msg_send![super(this), init] };
        let center = UNUserNotificationCenter::currentNotificationCenter();
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        NOTIFICATIONS.with(|s| {
            *s.borrow_mut() = Some(NativeNotifications {
                id,
                center: center.clone(),
                _delegate: delegate,
                authorized: None,
                pending: VecDeque::new(),
            })
        });
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            if let Some(error) = unsafe { error.as_ref() } {
                log::warn!("macOS notification authorization: {error}");
            } else if !granted.as_bool() {
                log::warn!("macOS notification permission denied; enable it in System Settings");
            }
            let granted = granted.as_bool() && error.is_null();
            on_main(move || {
                let pending = NOTIFICATIONS.with(|s| {
                    let mut state = s.borrow_mut();
                    let Some(native) = state.as_mut().filter(|n| n.id == id) else {
                        return VecDeque::new();
                    };
                    native.authorized = Some(granted);
                    std::mem::take(&mut native.pending)
                });
                if granted {
                    for notice in pending {
                        enqueue(id, notice);
                    }
                }
            });
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert,
            &completion,
        );
        Ok(Self {
            id,
            enabled: true,
            session,
        })
    }

    fn notify(
        &self,
        shot: Option<u64>,
        summary: &str,
        body: &str,
        thumbnail: Option<Vec<u8>>,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let serial = NEXT_NOTICE.fetch_add(1, Ordering::Relaxed);
        let identifier = match shot {
            Some(shot) => format!("quikscope-{}-shot-{shot}-{serial}", self.session),
            None => format!("quikscope-{}-message-{serial}", self.session),
        };
        let notice = Notice {
            identifier,
            summary: summary.into(),
            body: body.into(),
            thumbnail,
        };
        let id = self.id;
        on_main(move || enqueue(id, notice));
        Ok(())
    }
}
impl Notifier for Notifications {
    fn shot_captured(&self, shot: u64, image: &Image, body: &str, _timeout_ms: i32) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        // Notification Center owns banner duration and history retention. There
        // is no native per-notification timeout API. Never expire click actions.
        let thumbnail = match image.thumbnail(384).encode_png(Compression::Fast) {
            Ok(png) => Some(png),
            Err(error) => {
                log::warn!("encoding notification thumbnail: {error:#}");
                None
            }
        };
        self.notify(Some(shot), "Screenshot captured", body, thumbnail)
    }
    fn message(&self, summary: &str, body: &str) -> Result<()> {
        self.notify(None, summary, body, None)
    }
}
impl Drop for Notifications {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        let id = self.id;
        on_main(move || {
            NOTIFICATIONS.with(|s| {
                let mut state = s.borrow_mut();
                if state.as_ref().is_some_and(|n| n.id == id)
                    && let Some(native) = state.take()
                {
                    native.center.setDelegate(None);
                }
            })
        });
    }
}

fn enqueue(id: u64, notice: Notice) {
    let center = NOTIFICATIONS.with(|s| {
        let mut state = s.borrow_mut();
        let native = state.as_mut().filter(|n| n.id == id)?;
        match native.authorized {
            None => {
                // Permission prompts can remain open indefinitely; retain only
                // the most recent messages instead of accumulating captures.
                if native.pending.len() == 16 {
                    native.pending.pop_front();
                }
                native.pending.push_back(notice);
                None
            }
            Some(false) => None,
            Some(true) => Some((native.center.clone(), notice)),
        }
    });
    let Some((center, notice)) = center else {
        return;
    };
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notice.summary));
    content.setBody(&NSString::from_str(&notice.body));
    if let Some(png) = notice.thumbnail {
        match attachment(&notice.identifier, &png) {
            Ok(attachment) => content.setAttachments(&NSArray::from_slice(&[&*attachment])),
            Err(error) => log::warn!("attaching notification thumbnail: {error:#}"),
        }
    }
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&notice.identifier),
        &content,
        None,
    );
    let completion = RcBlock::new(|error: *mut NSError| {
        if let Some(error) = unsafe { error.as_ref() } {
            log::warn!("posting macOS notification: {error}");
        }
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&completion));
}

fn attachment(identifier: &str, png: &[u8]) -> Result<Retained<UNNotificationAttachment>> {
    // create_new and a private mode prevent other users from substituting or
    // reading screenshot thumbnails in the temporary directory.
    let path = std::env::temp_dir().join(format!("{identifier}-{}.png", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .context("creating private notification thumbnail")?;
    let result = (|| {
        file.write_all(png)
            .context("writing notification thumbnail")?;
        drop(file);
        let path_string = path
            .to_str()
            .context("notification thumbnail path is not UTF-8")?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path_string));
        // UserNotifications validates and imports the file synchronously here.
        unsafe {
            UNNotificationAttachment::attachmentWithIdentifier_URL_options_error(
                &NSString::from_str(identifier),
                &url,
                None,
            )
        }
        .map_err(|e| anyhow::anyhow!("importing notification thumbnail: {e}"))
    })();
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("removing notification thumbnail: {error}");
    }
    result
}
