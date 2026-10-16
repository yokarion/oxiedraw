use std::sync::atomic::{AtomicU64, Ordering};

use relm4::gtk::prelude::*;
use relm4::gtk::{gdk, gio, glib};

/// Internal layer clipboard. Holds full-canvas BGRA8 pixels plus enough
/// metadata to recreate the layer exactly. Used by copy/paste to preserve
/// all layer properties when pasting within the same app session.
#[derive(Clone)]
pub(crate) struct LayerClipboard {
    pub(crate) name: String,
    /// Premultiplied BGRA8, `canvas_width x canvas_height`, row-major.
    pub(crate) pixels: Vec<u8>,
    pub(crate) canvas_width: u32,
    pub(crate) canvas_height: u32,
    /// Also published on the system clipboard; a paste only uses this copy
    /// while the clipboard still carries the same token.
    pub(crate) token: String,
}

/// An image waiting to be decoded into a new layer on the paste worker.
pub(crate) enum PasteJob {
    Texture { texture: gdk::Texture, name: String },
    Encoded { bytes: glib::Bytes, name: String },
    File { uri: String },
}


// Clipboard managers re-offer every mime type, so the marker survives them,
// while any newer copy (ours or another app's) replaces it
const LAYER_MIME: &str = "application/x-oxiedraw-layer";
const URI_LIST_MIME: &str = "text/uri-list";
const PORTAL_MIMES: [&str; 2] = ["application/vnd.portal.filetransfer", "application/vnd.portal.files"];
// Formats gdk::Texture::from_bytes decodes without gdk-pixbuf
const IMAGE_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/tiff"];

// Browser drags list page links next to (or instead of) files; only local files load
pub(crate) fn file_jobs(uris: Vec<String>) -> Vec<PasteJob> {
    let (local, remote): (Vec<String>, Vec<String>) =
        uris.into_iter().partition(|uri| gio::File::for_uri(uri).is_native());
    if !remote.is_empty() {
        tracing::info!(?remote, "skipped non-local uris");
    }
    local.into_iter().map(|uri| PasteJob::File { uri }).collect()
}

pub(crate) fn next_layer_token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

pub(crate) fn layer_content(texture: &gdk::Texture, token: &str) -> gdk::ContentProvider {
    gdk::ContentProvider::new_union(&[
        gdk::ContentProvider::for_value(&texture.to_value()),
        gdk::ContentProvider::for_bytes(LAYER_MIME, &glib::Bytes::from(token.as_bytes())),
    ])
}

pub(crate) fn read_layer_token(clipboard: &gdk::Clipboard, done: impl FnOnce(Option<String>) + 'static) {
    if !clipboard.formats().contain_mime_type(LAYER_MIME) {
        done(None);
        return;
    }
    clipboard.read_async(&[LAYER_MIME], glib::Priority::DEFAULT, gio::Cancellable::NONE, move |result| {
        match result {
            Ok((stream, _)) => read_to_end(&stream, move |bytes| {
                done(bytes.ok().map(|b| String::from_utf8_lossy(&b).into_owned()));
            }),
            Err(_) => done(None),
        }
    });
}

/// Everything the canvas drop target takes: images, files and component ids.
pub(crate) fn drop_formats() -> gdk::ContentFormats {
    let mut builder = gdk::ContentFormats::builder().add_mime_type(URI_LIST_MIME);
    for mime in IMAGE_MIMES {
        builder = builder.add_mime_type(mime);
    }
    builder
        .add_type(gdk::FileList::static_type())
        .add_type(glib::types::Type::STRING)
        .build()
        .union_deserialize_mime_types()
}

/// Where files copied or dragged out of a file manager are read from.
pub(crate) enum FileSource {
    Clipboard(gdk::Clipboard),
    Drop(gdk::Drop),
}

impl FileSource {
    fn formats(&self) -> gdk::ContentFormats {
        match self {
            Self::Clipboard(c) => c.formats(),
            Self::Drop(d) => d.formats(),
        }
    }
}

// Checked before files: browsers put the page link in text/uri-list next to the image
pub(crate) fn offers_image_data(formats: &gdk::ContentFormats) -> bool {
    IMAGE_MIMES.iter().any(|m| formats.contain_mime_type(m))
}

pub(crate) fn read_dropped_image(
    drop: &gdk::Drop,
    done: impl FnOnce(Result<glib::Bytes, glib::Error>) + 'static,
) {
    drop.read_async(&IMAGE_MIMES, glib::Priority::DEFAULT, gio::Cancellable::NONE, move |result| {
        match result {
            Ok((stream, _)) => read_to_end(&stream, done),
            Err(e) => done(Err(e)),
        }
    });
}

pub(crate) fn offers_files(formats: &gdk::ContentFormats) -> bool {
    formats.contain_mime_type(URI_LIST_MIME)
        || PORTAL_MIMES.iter().any(|m| formats.contain_mime_type(m))
        || formats.contains_type(gdk::FileList::static_type())
}

// Plain uri-list first: GDK's FileList read prefers the document portal, which
// can fail with no error surfacing; the portal stays as the sandboxed fallback
pub(crate) fn read_file_uris(
    source: &FileSource,
    done: impl FnOnce(Result<Vec<String>, glib::Error>) + 'static,
) {
    if source.formats().contain_mime_type(URI_LIST_MIME) {
        let on_stream = move |result: Result<(gio::InputStream, glib::GString), glib::Error>| {
            match result {
                Ok((stream, _)) => read_to_end(&stream, move |bytes| {
                    done(bytes.map(|b| parse_uri_list(&String::from_utf8_lossy(&b))));
                }),
                Err(e) => done(Err(e)),
            }
        };
        match source {
            FileSource::Clipboard(c) => {
                c.read_async(&[URI_LIST_MIME], glib::Priority::DEFAULT, gio::Cancellable::NONE, on_stream);
            }
            FileSource::Drop(d) => {
                d.read_async(&[URI_LIST_MIME], glib::Priority::DEFAULT, gio::Cancellable::NONE, on_stream);
            }
        }
        return;
    }

    let on_value = move |result: Result<glib::Value, glib::Error>| {
        done(result.and_then(|value| {
            value
                .get::<gdk::FileList>()
                .map(|list| list.files().iter().map(|f| f.uri().to_string()).collect())
                .map_err(|e| glib::Error::new(gio::IOErrorEnum::InvalidData, &e.to_string()))
        }));
    };
    let file_list = gdk::FileList::static_type();
    match source {
        FileSource::Clipboard(c) => {
            c.read_value_async(file_list, glib::Priority::DEFAULT, gio::Cancellable::NONE, on_value);
        }
        FileSource::Drop(d) => {
            d.read_value_async(file_list, glib::Priority::DEFAULT, gio::Cancellable::NONE, on_value);
        }
    }
}

fn read_to_end(stream: &gio::InputStream, done: impl FnOnce(Result<glib::Bytes, glib::Error>) + 'static) {
    let sink = gio::MemoryOutputStream::new_resizable();
    let sink_c = sink.clone();
    sink.splice_async(
        stream,
        gio::OutputStreamSpliceFlags::CLOSE_SOURCE | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |result| done(result.map(|_| sink_c.steal_as_bytes())),
    );
}

fn parse_uri_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_skips_comments_and_crlf() {
        let text = "# comment\r\nfile:///tmp/a.png\r\n\r\nfile:///tmp/b%20c.jpg\r\n";
        assert_eq!(parse_uri_list(text), ["file:///tmp/a.png", "file:///tmp/b%20c.jpg"]);
    }
}
