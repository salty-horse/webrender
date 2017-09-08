/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{BorderRadius, ComplexClipRegion, ImageMask, ImageRendering};
use api::{DeviceIntRect, LayerPoint, LayerRect, LayerSize, LayerToWorldTransform, LocalClip};
use border::BorderCornerClipSource;
use freelist::{FreeList, FreeListHandle, WeakFreeListHandle};
use gpu_cache::GpuCache;
use mask_cache::MaskCacheInfo;
use resource_cache::ResourceCache;
use std::ops::Not;
use util::{extract_inner_rect_safe, TransformedRect};

const MAX_CLIP: f32 = 1000000.0;

pub type ClipStore = FreeList<ClipSources>;
pub type ClipSourcesHandle = FreeListHandle<ClipSources>;
pub type ClipSourcesWeakHandle = WeakFreeListHandle<ClipSources>;

#[derive(Clone, Debug)]
pub struct ClipRegion {
    pub origin: LayerPoint,
    pub main: LayerRect,
    pub image_mask: Option<ImageMask>,
    pub complex_clips: Vec<ComplexClipRegion>,
}

impl ClipRegion {
    pub fn create_for_clip_node(rect: LayerRect,
                                mut complex_clips: Vec<ComplexClipRegion>,
                                mut image_mask: Option<ImageMask>)
                                -> ClipRegion {
        // All the coordinates we receive are relative to the stacking context, but we want
        // to convert them to something relative to the origin of the clip.
        let negative_origin = -rect.origin.to_vector();
        if let Some(ref mut image_mask) = image_mask {
            image_mask.rect = image_mask.rect.translate(&negative_origin);
        }

        for complex_clip in complex_clips.iter_mut() {
            complex_clip.rect = complex_clip.rect.translate(&negative_origin);
        }

        ClipRegion {
            origin: rect.origin,
            main: LayerRect::new(LayerPoint::zero(), rect.size),
            image_mask,
            complex_clips,
        }
    }

    pub fn create_for_clip_node_with_local_clip(local_clip: &LocalClip) -> ClipRegion {
        let complex_clips = match local_clip {
            &LocalClip::Rect(_) => Vec::new(),
            &LocalClip::RoundedRect(_, ref region) => vec![region.clone()],
        };
        ClipRegion::create_for_clip_node(*local_clip.clip_rect(), complex_clips, None)
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ClipMode {
    Clip,           // Pixels inside the region are visible.
    ClipOut,        // Pixels outside the region are visible.
}

impl Not for ClipMode {
    type Output = ClipMode;

    fn not(self) -> ClipMode {
        match self {
            ClipMode::Clip => ClipMode::ClipOut,
            ClipMode::ClipOut => ClipMode::Clip
        }
    }
}

#[derive(Debug)]
pub enum ClipSource {
    Rectangle(LayerRect),
    RoundedRectangle(LayerRect, BorderRadius, ClipMode),
    Image(ImageMask),
    /// TODO(gw): This currently only handles dashed style
    /// clips, where the border style is dashed for both
    /// adjacent border edges. Expand to handle dotted style
    /// and different styles per edge.
    BorderCorner(BorderCornerClipSource),
}

impl From<ClipRegion> for ClipSources {
    fn from(region: ClipRegion) -> ClipSources {
        let mut clips = Vec::new();

        if let Some(info) = region.image_mask {
            clips.push(ClipSource::Image(info));
        }

        clips.push(ClipSource::Rectangle(region.main));

        for complex in region.complex_clips {
            clips.push(ClipSource::RoundedRectangle(complex.rect, complex.radii, ClipMode::Clip));
        }

        ClipSources::new(clips)
    }
}

#[derive(Debug)]
pub struct ClipSources {
    clips: Vec<ClipSource>,
    pub mask_cache_info: MaskCacheInfo,
    pub bounds: MaskBounds,
}

impl ClipSources {
    pub fn new(clips: Vec<ClipSource>) -> ClipSources {
        let mask_cache_info = MaskCacheInfo::new(&clips);

        ClipSources {
            clips,
            mask_cache_info,
            bounds: MaskBounds {
                inner: None,
                outer: None,
            },
        }
    }

    pub fn clips(&self) -> &[ClipSource] {
        &self.clips
    }

    pub fn update(&mut self,
                  layer_transform: &LayerToWorldTransform,
                  gpu_cache: &mut GpuCache,
                  resource_cache: &mut ResourceCache,
                  device_pixel_ratio: f32) {
        if self.clips.is_empty() {
            return;
        }

        // compute the local bounds
        if self.bounds.inner.is_none() {
            let mut local_rect = Some(LayerRect::new(LayerPoint::new(-MAX_CLIP, -MAX_CLIP),
                                                     LayerSize::new(2.0 * MAX_CLIP, 2.0 * MAX_CLIP)));
            let mut local_inner = local_rect;
            let mut has_clip_out = false;
            let mut has_border_clip = false;

            for source in &self.clips {
                match *source {
                    ClipSource::Image(ref mask) => {
                        if !mask.repeat {
                            local_rect = local_rect.and_then(|r| r.intersection(&mask.rect));
                        }
                        local_inner = None;
                    }
                    ClipSource::Rectangle(rect) => {
                        local_rect = local_rect.and_then(|r| r.intersection(&rect));
                        local_inner = local_inner.and_then(|r| r.intersection(&rect));
                    }
                    ClipSource::RoundedRectangle(ref rect, ref radius, mode) => {
                        // Once we encounter a clip-out, we just assume the worst
                        // case clip mask size, for now.
                        if mode == ClipMode::ClipOut {
                            has_clip_out = true;
                            break;
                        }

                        local_rect = local_rect.and_then(|r| r.intersection(rect));

                        let inner_rect = extract_inner_rect_safe(rect, radius);
                        local_inner = local_inner.and_then(|r| inner_rect.and_then(|ref inner| r.intersection(inner)));
                    }
                    ClipSource::BorderCorner{..} => {
                        has_border_clip = true;
                    }
                }
            }

            // Work out the type of mask geometry we have, based on the
            // list of clip sources above.
            self.bounds = if has_clip_out || has_border_clip {
                // For clip-out, the mask rect is not known.
                MaskBounds {
                    outer: None,
                    inner: Some(LayerRect::zero().into()),
                }
            } else {
                MaskBounds {
                    outer: Some(local_rect.unwrap_or(LayerRect::zero()).into()),
                    inner: Some(local_inner.unwrap_or(LayerRect::zero()).into()),
                }
            };
        }

        // update the screen bounds
        self.bounds.update(layer_transform, device_pixel_ratio);

        self.mask_cache_info.update(&self.clips, gpu_cache);

        for clip in &self.clips {
            if let ClipSource::Image(ref mask) = *clip {
                resource_cache.request_image(mask.image,
                                             ImageRendering::Auto,
                                             None,
                                             gpu_cache);
            }
        }
    }

    pub fn is_masking(&self) -> bool {
        self.mask_cache_info.is_masking()
    }
}

/// Represents a local rect and a device space
/// rectangles that are either outside or inside bounds.
#[derive(Clone, Debug, PartialEq)]
pub struct Geometry {
    pub local_rect: LayerRect,
    pub device_rect: DeviceIntRect,
}

impl From<LayerRect> for Geometry {
    fn from(local_rect: LayerRect) -> Self {
        Geometry {
            local_rect,
            device_rect: DeviceIntRect::zero(),
        }
    }
}

/// Depending on the complexity of the clip, we may either
/// know the outer and/or inner rect, or neither or these.
/// In the case of a clip-out, we currently set the mask
/// bounds to be unknown. This is conservative, but ensures
/// correctness. In the future we can make this a lot
/// more clever with some proper region handling.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskBounds {
    pub outer: Option<Geometry>,
    pub inner: Option<Geometry>,
}

impl MaskBounds {
    pub fn update(&mut self, transform: &LayerToWorldTransform, device_pixel_ratio: f32) {
        if let Some(ref mut outer) = self.outer {
            let transformed = TransformedRect::new(&outer.local_rect,
                                                   transform,
                                                   device_pixel_ratio);
            outer.device_rect = transformed.bounding_rect;
        }
        if let Some(ref mut inner) = self.inner {
            let transformed = TransformedRect::new(&inner.local_rect,
                                                   transform,
                                                   device_pixel_ratio);
            inner.device_rect = transformed.inner_rect;
        }
    }
}
