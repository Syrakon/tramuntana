/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use app_units::Au;
use malloc_size_of_derive::MallocSizeOf;
use servo_base::id::ScrollTreeNodeId;
use style::computed_values::mask_clip::single_value::T as MaskClip;
use style::computed_values::mask_composite::single_value::T as MaskComposite;
use style::computed_values::mask_mode::single_value::T as MaskMode;
use style::properties::ComputedValues;
use style::values::computed::basic_shape::{BasicShape, ClipPath};
use style::values::computed::image::{Gradient, Image};
use style::values::computed::position::Position;
use style::values::computed::{BackgroundRepeat, BackgroundSize, Color, LengthPercentage};
use style::values::generics::basic_shape::{GenericShapeRadius, ShapeBox, ShapeGeometryBox};
use style::values::generics::image::GradientItem;
use style::values::generics::position::GenericPositionOrAuto;
use webrender_api::BorderRadius;
use webrender_api::units::{LayoutRect, LayoutSideOffsets, LayoutSize};

use super::background::get_cyclic;
use super::conversions::ToWebRender;
use super::{BuilderForBoxFragment, compute_margin_box_radius, normalize_radii, offset_radii};
use crate::style_ext::ComputedValuesExt;

/// An identifier for a clip used during StackingContextTree construction. This is a simple index in
/// a [`ClipStore`]s vector of clips.
#[derive(Clone, Copy, Debug, Eq, Hash, MallocSizeOf, PartialEq)]
pub(crate) struct ClipId(pub usize);

impl ClipId {
    /// Equivalent to [`ClipChainId::INVALID`]. This means "no clip."
    pub(crate) const INVALID: ClipId = ClipId(usize::MAX);
}

/// All the information needed to create a clip on a WebRender display list. These are created at
/// two times: during `StackingContextTree` creation and during WebRender display list construction.
/// Only the former are stored in a [`ClipStore`].
#[derive(Clone, MallocSizeOf)]
pub(crate) struct Clip {
    pub id: ClipId,
    pub radii: BorderRadius,
    pub rect: LayoutRect,
    /// Whether this clip hides what is inside of its rounded rectangle instead of what is
    /// outside of it.
    pub clip_out: bool,
    pub parent_scroll_node_id: ScrollTreeNodeId,
    pub parent_clip_id: ClipId,
}

/// A simple vector of [`Clip`] that is built during `StackingContextTree` construction.
/// These are later turned into WebRender clips and clip chains during WebRender display
/// list construction.
#[derive(Clone, Default, MallocSizeOf)]
pub(crate) struct StackingContextTreeClipStore(pub Vec<Clip>);

impl StackingContextTreeClipStore {
    pub(super) fn get(&self, clip_id: ClipId) -> &Clip {
        &self.0[clip_id.0]
    }

    pub(crate) fn add(
        &mut self,
        radii: webrender_api::BorderRadius,
        rect: LayoutRect,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_id: ClipId,
    ) -> ClipId {
        self.add_with_mode(
            radii,
            rect,
            false, /* clip_out */
            parent_scroll_node_id,
            parent_clip_id,
        )
    }

    fn add_with_mode(
        &mut self,
        radii: webrender_api::BorderRadius,
        rect: LayoutRect,
        clip_out: bool,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_id: ClipId,
    ) -> ClipId {
        let id = ClipId(self.0.len());
        self.0.push(Clip {
            id,
            radii,
            rect,
            clip_out,
            parent_scroll_node_id,
            parent_clip_id,
        });
        id
    }

    /// Adds the clips that `clip-path` and `mask` put on a fragment, the mask inside of the
    /// clip path. Returns `None` when neither of them clips anything.
    pub(super) fn add_for_clip_path_and_mask(
        &mut self,
        style: &ComputedValues,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_id: ClipId,
        fragment_builder: BuilderForBoxFragment,
    ) -> Option<ClipId> {
        let clip_path_clip_id = self.add_for_clip_path(
            style.clone_clip_path(),
            parent_scroll_node_id,
            parent_clip_id,
            &fragment_builder,
        );
        let mask_clip_id = self.add_for_mask(
            style,
            parent_scroll_node_id,
            clip_path_clip_id.unwrap_or(parent_clip_id),
            &fragment_builder,
        );
        mask_clip_id.or(clip_path_clip_id)
    }

    /// Adds the clips for a `mask` that only shows or hides whole boxes of the fragment.
    ///
    /// A mask layer whose image is opaque everywhere shows the fragment inside of the box that
    /// its `mask-clip` names and hides it outside, so compositing such layers yields plain
    /// geometry. This covers the usual way of drawing a gradient border: `mask:
    /// linear-gradient(#fff 0 0) content-box, linear-gradient(#fff 0 0)` together with
    /// `mask-composite: exclude`.
    ///
    /// TODO: Masks that depend on the pixels of their images are not painted.
    fn add_for_mask(
        &mut self,
        style: &ComputedValues,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_id: ClipId,
        fragment_builder: &BuilderForBoxFragment,
    ) -> Option<ClipId> {
        let bands = opaque_mask_bands(style)?;
        if bands == 0 {
            return Some(self.add(
                BorderRadius::zero(),
                LayoutRect::from_origin_and_size(
                    fragment_builder.border_rect.min,
                    LayoutSize::zero(),
                ),
                parent_scroll_node_id,
                parent_clip_id,
            ));
        }

        // The bands have to be next to each other: a rounded rectangle with a smaller one cut
        // out of it is all that a chain of clips can describe.
        let innermost = bands.trailing_zeros();
        let outermost = u8::BITS - 1 - bands.leading_zeros();
        if !((bands >> innermost) + 1).is_power_of_two() {
            return None;
        }

        let fragment = fragment_builder.fragment;
        let box_for_band = |band: u32| match band {
            0 => (
                *fragment_builder.content_rect(),
                offset_radii(
                    fragment_builder.border_radius,
                    -(fragment.border + fragment.padding).to_webrender(),
                ),
            ),
            1 => (
                *fragment_builder.padding_rect(),
                offset_radii(
                    fragment_builder.border_radius,
                    -fragment.border.to_webrender(),
                ),
            ),
            _ => (fragment_builder.border_rect, fragment_builder.border_radius),
        };

        let (rect, radii) = box_for_band(outermost);
        let clip_id = self.add(radii, rect, parent_scroll_node_id, parent_clip_id);
        if innermost == 0 {
            return Some(clip_id);
        }
        let (rect, radii) = box_for_band(innermost - 1);
        Some(self.add_with_mode(
            radii,
            rect,
            true, /* clip_out */
            parent_scroll_node_id,
            clip_id,
        ))
    }

    fn add_for_clip_path(
        &mut self,
        clip_path: ClipPath,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_chain_id: ClipId,
        fragment_builder: &BuilderForBoxFragment,
    ) -> Option<ClipId> {
        let geometry_box = match clip_path {
            ClipPath::Shape(_, ShapeGeometryBox::ShapeBox(shape_box)) => shape_box,
            ClipPath::Shape(_, ShapeGeometryBox::ElementDependent) => ShapeBox::BorderBox,
            ClipPath::Box(ShapeGeometryBox::ShapeBox(shape_box)) => shape_box,
            ClipPath::Box(ShapeGeometryBox::ElementDependent) => ShapeBox::BorderBox,
            _ => return None,
        };
        let layout_rect = match geometry_box {
            ShapeBox::BorderBox => fragment_builder.border_rect,
            ShapeBox::ContentBox => *fragment_builder.content_rect(),
            ShapeBox::PaddingBox => *fragment_builder.padding_rect(),
            ShapeBox::MarginBox => *fragment_builder.margin_rect(),
        };
        if let ClipPath::Shape(shape, _) = clip_path {
            match *shape {
                BasicShape::Circle(_) | BasicShape::Ellipse(_) | BasicShape::Rect(_) => self
                    .add_for_basic_shape(
                        *shape,
                        layout_rect,
                        parent_scroll_node_id,
                        parent_clip_chain_id,
                    ),
                BasicShape::Polygon(_) | BasicShape::PathOrShape(_) => None,
            }
        } else {
            Some(self.add(
                match geometry_box {
                    ShapeBox::MarginBox => compute_margin_box_radius(
                        fragment_builder.border_radius,
                        layout_rect.size(),
                        fragment_builder.fragment,
                    ),
                    _ => fragment_builder.border_radius,
                },
                layout_rect,
                parent_scroll_node_id,
                parent_clip_chain_id,
            ))
        }
    }

    #[servo_tracing::instrument(name = "StackingContextClipStore::add_for_basic_shape", skip_all)]
    fn add_for_basic_shape(
        &mut self,
        shape: BasicShape,
        layout_box: LayoutRect,
        parent_scroll_node_id: ScrollTreeNodeId,
        parent_clip_chain_id: ClipId,
    ) -> Option<ClipId> {
        match shape {
            BasicShape::Rect(rect) => {
                let box_height = Au::from_f32_px(layout_box.height());
                let box_width = Au::from_f32_px(layout_box.width());
                let insets = LayoutSideOffsets::new(
                    rect.rect.0.to_used_value(box_height).to_f32_px(),
                    rect.rect.1.to_used_value(box_width).to_f32_px(),
                    rect.rect.2.to_used_value(box_height).to_f32_px(),
                    rect.rect.3.to_used_value(box_width).to_f32_px(),
                );

                // `inner_rect()` will cause an assertion failure if the insets are larger than the
                // rectangle dimension.
                let shape_rect = if insets.left + insets.right >= layout_box.width() ||
                    insets.top + insets.bottom > layout_box.height()
                {
                    LayoutRect::from_origin_and_size(layout_box.min, LayoutSize::zero())
                } else {
                    layout_box.to_rect().inner_rect(insets).to_box2d()
                };

                let corner = |corner: &style::values::computed::BorderCornerRadius| {
                    LayoutSize::new(
                        corner.0.width.0.to_used_value(box_width).to_f32_px(),
                        corner.0.height.0.to_used_value(box_height).to_f32_px(),
                    )
                };
                let mut radii = webrender_api::BorderRadius {
                    top_left: corner(&rect.round.top_left),
                    top_right: corner(&rect.round.top_right),
                    bottom_left: corner(&rect.round.bottom_left),
                    bottom_right: corner(&rect.round.bottom_right),
                };
                normalize_radii(&layout_box, &mut radii);
                Some(self.add(
                    radii,
                    shape_rect,
                    parent_scroll_node_id,
                    parent_clip_chain_id,
                ))
            },
            BasicShape::Circle(circle) => {
                let center = match circle.position {
                    GenericPositionOrAuto::Position(position) => position,
                    GenericPositionOrAuto::Auto => Position::center(),
                };
                let anchor_x = center
                    .horizontal
                    .to_used_value(Au::from_f32_px(layout_box.width()));
                let anchor_y = center
                    .vertical
                    .to_used_value(Au::from_f32_px(layout_box.height()));
                let center = layout_box
                    .min
                    .add_size(&LayoutSize::new(anchor_x.to_f32_px(), anchor_y.to_f32_px()));

                let horizontal = compute_shape_radius(
                    center.x,
                    &circle.radius,
                    layout_box.min.x,
                    layout_box.max.x,
                );
                let vertical = compute_shape_radius(
                    center.y,
                    &circle.radius,
                    layout_box.min.y,
                    layout_box.max.y,
                );

                // If the value is `Length` then both values should be the same at this point.
                let radius = match circle.radius {
                    GenericShapeRadius::FarthestSide => horizontal.max(vertical),
                    GenericShapeRadius::ClosestSide => horizontal.min(vertical),
                    GenericShapeRadius::Length(_) => horizontal,
                };
                let radius = LayoutSize::new(radius, radius);
                let mut radii = webrender_api::BorderRadius {
                    top_left: radius,
                    top_right: radius,
                    bottom_left: radius,
                    bottom_right: radius,
                };
                let start = center.add_size(&-radius);
                let rect = LayoutRect::from_origin_and_size(start, radius * 2.);
                normalize_radii(&layout_box, &mut radii);
                Some(self.add(radii, rect, parent_scroll_node_id, parent_clip_chain_id))
            },
            BasicShape::Ellipse(ellipse) => {
                let center = match ellipse.position {
                    GenericPositionOrAuto::Position(position) => position,
                    GenericPositionOrAuto::Auto => Position::center(),
                };
                let anchor_x = center
                    .horizontal
                    .to_used_value(Au::from_f32_px(layout_box.width()));
                let anchor_y = center
                    .vertical
                    .to_used_value(Au::from_f32_px(layout_box.height()));
                let center = layout_box
                    .min
                    .add_size(&LayoutSize::new(anchor_x.to_f32_px(), anchor_y.to_f32_px()));

                let width = compute_shape_radius(
                    center.x,
                    &ellipse.semiaxis_x,
                    layout_box.min.x,
                    layout_box.max.x,
                );
                let height = compute_shape_radius(
                    center.y,
                    &ellipse.semiaxis_y,
                    layout_box.min.y,
                    layout_box.max.y,
                );

                let mut radii = webrender_api::BorderRadius {
                    top_left: LayoutSize::new(width, height),
                    top_right: LayoutSize::new(width, height),
                    bottom_left: LayoutSize::new(width, height),
                    bottom_right: LayoutSize::new(width, height),
                };
                let size = LayoutSize::new(width, height);
                let start = center.add_size(&-size);
                let rect = LayoutRect::from_origin_and_size(start, size * 2.);
                normalize_radii(&rect, &mut radii);
                Some(self.add(radii, rect, parent_scroll_node_id, parent_clip_chain_id))
            },
            _ => None,
        }
    }
}

fn compute_shape_radius(
    center: f32,
    radius: &GenericShapeRadius<LengthPercentage>,
    min_edge: f32,
    max_edge: f32,
) -> f32 {
    let distance_from_min_edge = (min_edge - center).abs();
    let distance_from_max_edge = (max_edge - center).abs();
    match radius {
        GenericShapeRadius::FarthestSide => distance_from_min_edge.max(distance_from_max_edge),
        GenericShapeRadius::ClosestSide => distance_from_min_edge.min(distance_from_max_edge),
        GenericShapeRadius::Length(length) => length
            .to_used_value(Au::from_f32_px(max_edge - min_edge))
            .to_f32_px(),
    }
}

/// The part of the content box that a mask shows.
const CONTENT_BAND: u8 = 0b001;
/// The part of the padding box outside of the content box that a mask shows.
const PADDING_BAND: u8 = 0b010;
/// The part of the border box outside of the padding box that a mask shows.
const BORDER_BAND: u8 = 0b100;

/// The bands of the fragment that the `mask` of this style shows, when every one of its layers
/// is opaque over the whole box that it is clipped to. `None` when there is no mask or when the
/// mask depends on the pixels of its images.
fn opaque_mask_bands(style: &ComputedValues) -> Option<u8> {
    if !style.has_mask_image() {
        return None;
    }

    let svg = style.get_svg();
    let layers = &svg.mask_image.0;
    let mut bands = 0;
    for (index, image) in layers.iter().enumerate().rev() {
        let Image::Gradient(gradient) = image else {
            return None;
        };
        let is_opaque = match gradient.as_ref() {
            Gradient::Linear { items, .. } | Gradient::Radial { items, .. } => {
                gradient_is_opaque(style, items)
            },
            Gradient::Conic { items, .. } => gradient_is_opaque(style, items),
        };
        // An image that is tiled at its natural size covers the whole box, which is the only
        // arrangement where the position and the origin of the layer do not matter.
        if !is_opaque ||
            *get_cyclic(&svg.mask_mode.0, index) == MaskMode::Luminance ||
            *get_cyclic(&svg.mask_size.0, index) != BackgroundSize::auto() ||
            *get_cyclic(&svg.mask_repeat.0, index) != BackgroundRepeat::repeat()
        {
            return None;
        }

        let layer = match *get_cyclic(&svg.mask_clip.0, index) {
            MaskClip::ContentBox => CONTENT_BAND,
            MaskClip::PaddingBox => CONTENT_BAND | PADDING_BAND,
            MaskClip::BorderBox => CONTENT_BAND | PADDING_BAND | BORDER_BAND,
        };
        // `mask-composite` combines a layer with the layers below it, so the last layer has
        // nothing to be combined with.
        bands = if index + 1 == layers.len() {
            layer
        } else {
            match *get_cyclic(&svg.mask_composite.0, index) {
                MaskComposite::Add => layer | bands,
                MaskComposite::Subtract => layer & !bands,
                MaskComposite::Intersect => layer & bands,
                MaskComposite::Exclude => layer ^ bands,
            }
        };
    }
    Some(bands)
}

fn gradient_is_opaque<T>(style: &ComputedValues, items: &[GradientItem<Color, T>]) -> bool {
    items.iter().all(|item| match item {
        GradientItem::SimpleColorStop(color) | GradientItem::ComplexColorStop { color, .. } => {
            style.resolve_color(color).alpha == 1.0
        },
        GradientItem::InterpolationHint(_) => true,
    })
}
