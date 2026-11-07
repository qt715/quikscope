//! NSPasteboard promises PNG immediately; paste waits only for the shared encoder.
use crate::{clipboard::PendingPng, platform::Clipboard, raster::Image};
use objc2::{
    AnyThread, DefinedClass, define_class, msg_send, rc::Retained, runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSImage, NSPasteboard, NSPasteboardItem, NSPasteboardItemDataProvider, NSPasteboardTypePNG,
    NSPasteboardTypeString, NSPasteboardTypeTIFF, NSPasteboardWriting,
};
use objc2_foundation::{NSArray, NSData, NSObject, NSObjectProtocol, NSString};
use std::sync::Arc;

pub struct Pasteboard;

impl Clipboard for Pasteboard {
    fn set_image(&self, image: Arc<Image>) -> Arc<PendingPng> {
        let png = PendingPng::encode(image);
        let promised = png.clone();
        // Pasteboard data-provider requests are serviced by the owner's runloop.
        // A capture/encoding worker has no Cocoa runloop, so claim on GPUI's.
        super::shell::on_main(move || {
            let provider = ImageProvider::new(promised);
            let item = NSPasteboardItem::new();
            let types =
                unsafe { NSArray::from_slice(&[NSPasteboardTypePNG, NSPasteboardTypeTIFF]) };
            item.setDataProvider_forTypes(ProtocolObject::from_ref(&*provider), &types);
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            let object: &ProtocolObject<dyn NSPasteboardWriting> = ProtocolObject::from_ref(&*item);
            if !board.writeObjects(&NSArray::from_slice(&[object])) {
                log::error!("macOS pasteboard rejected screenshot");
            }
            // NSPasteboardItem retains its data provider until ownership changes.
        });
        png
    }
    fn set_text(&self, text: &str) {
        let text = text.to_owned();
        super::shell::on_main(move || {
            let board = NSPasteboard::generalPasteboard();
            board.clearContents();
            if !board.setString_forType(&NSString::from_str(&text), unsafe {
                NSPasteboardTypeString
            }) {
                log::error!("macOS pasteboard rejected text");
            }
        });
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = Arc<PendingPng>]
    struct ImageProvider;
    unsafe impl NSObjectProtocol for ImageProvider {}
    unsafe impl NSPasteboardItemDataProvider for ImageProvider {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide(&self, _board: Option<&NSPasteboard>, item: &NSPasteboardItem, kind: &NSString) {
            let bytes = match self.ivars().wait() {
                Ok(bytes) => bytes,
                Err(error) => { log::error!("encoding clipboard PNG: {error}"); return; }
            };
            let png = NSData::with_bytes(bytes);
            let data = if kind == unsafe { NSPasteboardTypeTIFF } {
                NSImage::initWithData(NSImage::alloc(), &png).and_then(|image| image.TIFFRepresentation())
            } else { Some(png) };
            if let Some(data) = data { item.setData_forType(&data, kind); }
        }
    }
);
impl ImageProvider {
    fn new(png: Arc<PendingPng>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(png);
        unsafe { msg_send![super(this), init] }
    }
}
