/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use ron;
use std::borrow::BorrowMut;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::{fmt, fs};
use super::CURRENT_FRAME_NUMBER;
use webrender;
use webrender::api::*;
use webrender::api::channel::Payload;

enum CachedFont {
    Native(NativeFontHandle),
    Raw(Option<Vec<u8>>, u32, Option<PathBuf>),
}

struct CachedImage {
    width: u32,
    height: u32,
    format: ImageFormat,
    bytes: Option<Vec<u8>>,
    path: Option<PathBuf>,
}

pub struct RonFrameWriter {
    frame_base: PathBuf,
    images: HashMap<ImageKey, CachedImage>,
    fonts: HashMap<FontKey, CachedFont>,

    last_frame_written: u32,

    dl_descriptor: Option<BuiltDisplayListDescriptor>,
}

impl RonFrameWriter {
    pub fn new(path: &Path) -> Self {
        let mut rsrc_base = path.to_owned();
        rsrc_base.push("res");
        fs::create_dir_all(&rsrc_base).ok();

        RonFrameWriter {
            frame_base: path.to_owned(),
            images: HashMap::new(),
            fonts: HashMap::new(),

            dl_descriptor: None,

            last_frame_written: u32::max_value(),
        }
    }

    pub fn begin_write_display_list(
        &mut self,
        _: &Epoch,
        _: &PipelineId,
        _: &Option<ColorF>,
        _: &LayoutSize,
        display_list: &BuiltDisplayListDescriptor,
    ) {
        unsafe {
            if CURRENT_FRAME_NUMBER == self.last_frame_written {
                return;
            }
            self.last_frame_written = CURRENT_FRAME_NUMBER;
        }

        self.dl_descriptor = Some(display_list.clone());
    }

    pub fn finish_write_display_list(&mut self, _frame: u32, data: &[u8]) {
        let payload = Payload::from_data(data);
        let dl_desc = self.dl_descriptor.take().unwrap();

        let dl = BuiltDisplayList::from_data(payload.display_list_data, dl_desc);

        let mut frame_file_name = self.frame_base.clone();
        let current_shown_frame = unsafe { CURRENT_FRAME_NUMBER };
        frame_file_name.push(format!("frame-{}.ron", current_shown_frame));

        let mut file = fs::File::create(&frame_file_name).unwrap();

        let s = ron::ser::pretty::to_string(&dl).unwrap();
        file.write_all(&s.into_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
    }

    fn update_resources(&mut self, updates: &ResourceUpdates) {
        for update in &updates.updates {
            match *update {
                ResourceUpdate::AddImage(ref img) => {
                    let bytes = match img.data {
                        ImageData::Raw(ref v) => (**v).clone(),
                        ImageData::External(_) | ImageData::Blob(_) => {
                            return;
                        }
                    };
                    self.images.insert(
                        img.key,
                        CachedImage {
                            width: img.descriptor.width,
                            height: img.descriptor.height,
                            format: img.descriptor.format,
                            bytes: Some(bytes),
                            path: None,
                        },
                    );
                }
                ResourceUpdate::UpdateImage(ref img) => {
                    if let Some(ref mut data) = self.images.get_mut(&img.key) {
                        assert_eq!(data.width, img.descriptor.width);
                        assert_eq!(data.height, img.descriptor.height);
                        assert_eq!(data.format, img.descriptor.format);

                        if let ImageData::Raw(ref bytes) = img.data {
                            *data.path.borrow_mut() = None;
                            *data.bytes.borrow_mut() = Some((**bytes).clone());
                        } else {
                            // Other existing image types only make sense within the gecko integration.
                            println!(
                                "Wrench only supports updating buffer images ({}).",
                                "ignoring update commands"
                            );
                        }
                    }
                }
                ResourceUpdate::DeleteImage(img) => {
                    self.images.remove(&img);
                }
                ResourceUpdate::AddFont(ref font) => match font {
                    &AddFont::Raw(key, ref bytes, index) => {
                        self.fonts
                            .insert(key, CachedFont::Raw(Some(bytes.clone()), index, None));
                    }
                    &AddFont::Native(key, ref handle) => {
                        self.fonts.insert(key, CachedFont::Native(handle.clone()));
                    }
                },
                ResourceUpdate::DeleteFont(_) => {}
                ResourceUpdate::AddFontInstance(_) => {}
                ResourceUpdate::DeleteFontInstance(_) => {}
            }
        }
    }
}

impl fmt::Debug for RonFrameWriter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RonFrameWriter")
    }
}

impl webrender::ApiRecordingReceiver for RonFrameWriter {
    fn write_msg(&mut self, _: u32, msg: &ApiMsg) {
        match *msg {
            ApiMsg::UpdateResources(ref updates) => self.update_resources(updates),

            ApiMsg::UpdateDocument(
                _,
                DocumentMsg::SetDisplayList {
                    ref epoch,
                    ref pipeline_id,
                    ref background,
                    ref viewport_size,
                    ref list_descriptor,
                    ref resources,
                    ..
                },
            ) => {
                self.update_resources(resources);
                self.begin_write_display_list(
                    epoch,
                    pipeline_id,
                    background,
                    viewport_size,
                    list_descriptor,
                );
            }
            ApiMsg::CloneApi(..) => {}
            _ => {}
        }
    }

    fn write_payload(&mut self, frame: u32, data: &[u8]) {
        if self.dl_descriptor.is_some() {
            self.finish_write_display_list(frame, data);
        }
    }
}
