/* Fundamental layout structures and algorithms. */

use servo_util::color::rgb;
use arc = std::arc;
use arc::ARC;
use au = gfx::geometry;
use au::au;
use core::dvec::DVec;
use core::to_str::ToStr;
use core::rand;
use css::styles::SpecifiedStyle;
use css::values::{BoxSizing, Length, Px, CSSDisplay, Specified, BgColor, BgColorTransparent, BdrColor, PosAbsolute};
use dl = gfx::display_list;
use dom::element::{ElementKind, HTMLDivElement, HTMLImageElement};
use dom::node::{Element, Node, NodeData, NodeKind, NodeTree};
use geom::rect::Rect;
use geom::size::Size2D;
use geom::point::Point2D;
use image::{Image, ImageHolder};
use layout::context::LayoutContext;
use layout::debug::BoxedDebugMethods;
use layout::flow::FlowContext;
use layout::text::TextBoxData;
use servo_text::text_run;
use servo_text::text_run::TextRun;
use std::net::url::Url;
use task::spawn;
use util::color::Color;
use util::tree;

/** 
Render boxes (`struct RenderBox`) are the leafs of the layout
tree. They cannot position themselves. In general, render boxes do not
have a simple correspondence with CSS boxes as in the specification:

 * Several render boxes may correspond to the same CSS box or DOM
   node. For example, a CSS text box broken across two lines is
   represented by two render boxes.

 * Some CSS boxes are not created at all, such as some anonymous block
   boxes induced by inline boxes with block-level sibling boxes. In
   that case, Servo uses an InlineFlow with BlockFlow siblings; the
   InlineFlow is block-level, but not a block container. It is
   positioned as if it were a block box, but its children are
   positioned according to inline flow.

Fundamental box types include:

 * GenericBox: an empty box that contributes only borders, margins,
padding, backgrounds. It is analogous to a CSS nonreplaced content box.

 * ImageBox: a box that represents a (replaced content) image and its
   accompanying borders, shadows, etc.

 * TextBox: a box representing a single run of text with a distinct
   style. A TextBox may be split into two or more render boxes across
   line breaks. Several TextBoxes may correspond to a single DOM text
   node. Split text boxes are implemented by referring to subsets of a
   master TextRun object.

*/

/* A box's kind influences how its styles are interpreted during
   layout.  For example, replaced content such as images are resized
   differently than tables, text, or other content.

   It also holds data specific to different box types, such as text.
*/
struct RenderBoxData {
    /* originating DOM node */
    node : Node,
    /* reference to containing flow context, which this box
       participates in */
    ctx  : @FlowContext,
    /* position of this box relative to owning flow */
    mut position : Rect<au>,
    font_size : Length,
    /* TODO (Issue #87): debug only */
    mut id: int
}

enum RenderBoxType {
    RenderBox_Generic,
    RenderBox_Image,
    RenderBox_Text,
}

pub enum RenderBox {
    GenericBox(RenderBoxData),
    ImageBox(RenderBoxData, ImageHolder),
    TextBox(RenderBoxData, TextBoxData),
    UnscannedTextBox(RenderBoxData, ~str)
}

pub enum SplitBoxResult {
    CannotSplit(@RenderBox),
    // in general, when splitting the left or right side can
    // be zero length, due to leading/trailing trimmable whitespace
    SplitDidFit(Option<@RenderBox>, Option<@RenderBox>),
    SplitDidNotFit(Option<@RenderBox>, Option<@RenderBox>)
}

enum InlineSpacerSide {
    LogicalBefore,
    LogicalAfter,
}

trait RenderBoxMethods {
    pure fn d(&self) -> &self/RenderBoxData;

    pure fn is_replaced() -> bool;
    pure fn can_split() -> bool;
    pure fn is_whitespace_only() -> bool;
    pure fn can_merge_with_box(@self, other: @RenderBox) -> bool;
    pure fn requires_inline_spacers() -> bool;
    pure fn content_box() -> Rect<au>;
    pure fn border_box() -> Rect<au>;
    pure fn margin_box() -> Rect<au>;

    fn split_to_width(@self, &LayoutContext, au, starts_line: bool) -> SplitBoxResult;
    fn get_min_width(&LayoutContext) -> au;
    fn get_pref_width(&LayoutContext) -> au;
    fn get_used_width() -> (au, au);
    fn get_used_height() -> (au, au);
    fn create_inline_spacer_for_side(&LayoutContext, InlineSpacerSide) -> Option<@RenderBox>;
    fn build_display_list(@self, &dl::DisplayListBuilder, dirty: &Rect<au>, 
                          offset: &Point2D<au>, &dl::DisplayList);
}

fn RenderBoxData(node: Node, ctx: @FlowContext, id: int) -> RenderBoxData {
    RenderBoxData {
        node : node,
        mut ctx  : ctx,
        mut position : au::zero_rect(),
        font_size: Px(0.0),
        id : id
    }
}

impl RenderBox : RenderBoxMethods {
    pure fn d(&self) -> &self/RenderBoxData {
        match *self {
            GenericBox(ref d)  => d,
            ImageBox(ref d, _) => d,
            TextBox(ref d, _)  => d,
            UnscannedTextBox(ref d, _) => d,
        }
    }

    pure fn is_replaced() -> bool {
        match self {
           ImageBox(*) => true, // TODO: form elements, etc
            _ => false
        }
    }

    pure fn can_split() -> bool {
        match self {
            TextBox(*) => true,
            _ => false
        }
    }

    pure fn is_whitespace_only() -> bool {
        match self {
            UnscannedTextBox(_, raw_text) => raw_text.is_whitespace(),
            _ => false
        }
    }

    pure fn can_merge_with_box(@self, other: @RenderBox) -> bool {
        assert !core::box::ptr_eq(self, other);

        match (self, other) {
            (@UnscannedTextBox(*), @UnscannedTextBox(*)) => true,
            (@TextBox(_,d1), @TextBox(_,d2)) => { core::box::ptr_eq(d1.run, d2.run) }
            (_, _) => false
        }
    }

    fn split_to_width(@self, _ctx: &LayoutContext, max_width: au, starts_line: bool) -> SplitBoxResult {
        match self {
            @GenericBox(*) => CannotSplit(self),
            @ImageBox(*) => CannotSplit(self),
            @UnscannedTextBox(*) => fail ~"WAT: shouldn't be an unscanned text box here.",
            @TextBox(_,data) => {

                let mut i : uint = 0;
                let mut remaining_width : au = max_width;
                let mut left_offset : uint = data.offset;
                let mut left_length : uint = 0;
                let mut right_offset : Option<uint> = None;
                let mut right_length : Option<uint> = None;
                debug!("split_to_width: splitting text box (strlen=%u, off=%u, len=%u, avail_width=%?)",
                       data.run.text.len(), data.offset, data.length, max_width);
                do data.run.iter_indivisible_pieces_for_range(data.offset, data.length) |off, len| {
                    debug!("split_to_width: considering range (off=%u, len=%u, remain_width=%?)",
                           off, len, remaining_width);
                    let metrics = data.run.metrics_for_range(off, len);
                    let advance = metrics.advance_width;
                    let should_continue : bool;

                    if advance <= remaining_width {
                        should_continue = true;
                        if starts_line && i == 0 && data.run.range_is_trimmable_whitespace(off, len) {
                            debug!("split_to_width: case=skipping leading trimmable whitespace");
                            left_offset += len; 
                        } else {
                            debug!("split_to_width: case=enlarging span");
                            remaining_width -= advance;
                            left_length += len;
                        }
                    } else { /* advance > remaining_width */
                        should_continue = false;

                        if data.run.range_is_trimmable_whitespace(off, len) {
                            // if there are still things after the trimmable whitespace, create right chunk
                            if off + len < data.offset + data.length {
                                debug!("split_to_width: case=skipping trimmable trailing whitespace, then split remainder");
                                right_offset = Some(off + len);
                                right_length = Some((data.offset + data.length) - (off + len));
                            } else {
                                debug!("split_to_width: case=skipping trimmable trailing whitespace");
                            }
                        } else if off < data.length + data.offset {
                            // still things left, create right chunk
                            right_offset = Some(off);
                            right_length = Some((data.offset + data.length) - off);
                            debug!("split_to_width: case=splitting remainder with right span: (off=%u, len=%u)",
                                   off, (data.offset + data.length) - off);
                        }
                    }
                    i += 1;
                    should_continue
                }

                let left_box = if left_length > 0 {
                    Some(layout::text::adapt_textbox_with_range(self.d(), data.run,
                                                                left_offset, left_length))
                } else { None };
                
                match (right_offset, right_length) {
                    (Some(right_off), Some(right_len)) => {
                        let right_box = layout::text::adapt_textbox_with_range(self.d(), data.run,
                                                                               right_off, right_len);
                        return if i == 1 || left_box.is_none() {
                            SplitDidNotFit(left_box, Some(right_box))
                        } else {
                            SplitDidFit(left_box, Some(right_box))
                        }
                    },
                    (_, _) => { return SplitDidFit(left_box, None); },
                }
            },
        }
    }

    /** In general, these functions are transitively impure because they
     * may cause glyphs to be allocated. For now, it's impure because of 
     * holder.get_image()
    */
    fn get_min_width(_ctx: &LayoutContext) -> au {
        match self {
            // TODO: this should account for min/pref widths of the
            // box element in isolation. That includes
            // border/margin/padding but not child widths. The block
            // FlowContext will combine the width of this element and
            // that of its children to arrive at the context width.
            GenericBox(*) => au(0),
            // TODO: consult CSS 'width', margin, border.
            // TODO: If image isn't available, consult 'width'.
            ImageBox(_,i) => au::from_px(i.get_size().get_default(Size2D(0,0)).width),
            TextBox(_,d) => d.run.min_width_for_range(d.offset, d.length),
            UnscannedTextBox(*) => fail ~"Shouldn't see unscanned boxes here."
        }
    }

    fn get_pref_width(_ctx: &LayoutContext) -> au {
        match self {
            // TODO: this should account for min/pref widths of the
            // box element in isolation. That includes
            // border/margin/padding but not child widths. The block
            // FlowContext will combine the width of this element and
            // that of its children to arrive at the context width.
            GenericBox(*) => au(0),
            ImageBox(_,i) => au::from_px(i.get_size().get_default(Size2D(0,0)).width),

            // a text box cannot span lines, so assume that this is an unsplit text box.

            // TODO: If text boxes have been split to wrap lines, then
            // they could report a smaller pref width during incremental reflow.
            // maybe text boxes should report nothing, and the parent flow could
            // factor in min/pref widths of any text runs that it owns.
            TextBox(_,d) => {
                let mut max_line_width: au = au(0);
                for d.run.iter_natural_lines_for_range(d.offset, d.length) |line_offset, line_len| {
                    // if the line is a single newline, then len will be zero
                    if line_len == 0 { loop }

                    let mut line_width: au = au(0);
                    do d.run.glyphs.iter_glyphs_for_range(line_offset, line_len) |_char_i, glyph| {
                        line_width += glyph.advance()
                    };
                    max_line_width = au::max(max_line_width, line_width);
                }

                max_line_width
            },
            UnscannedTextBox(*) => fail ~"Shouldn't see unscanned boxes here."
        }
    }

    /* Returns the amount of left, right "fringe" used by this
    box. This should be based on margin, border, padding, width. */
    fn get_used_width() -> (au, au) {
        // TODO: this should actually do some computation!
        // See CSS 2.1, Section 10.3, 10.4.

        (au(0), au(0))
    }
    
    /* Returns the amount of left, right "fringe" used by this
    box. This should be based on margin, border, padding, width. */
    fn get_used_height() -> (au, au) {
        // TODO: this should actually do some computation!
        // See CSS 2.1, Section 10.5, 10.6.

        (au(0), au(0))
    }

    /* Whether "spacer" boxes are needed to stand in for this DOM node */
    pure fn requires_inline_spacers() -> bool {
        return false;
    }

    /* The box formed by the content edge, as defined in CSS 2.1 Section 8.1.
       Coordinates are relative to the owning flow. */
    pure fn content_box() -> Rect<au> {
        match self {
            ImageBox(_,i) => {
                let size = i.size();
                Rect {
                    origin: copy self.d().position.origin,
                    size:   Size2D(au::from_px(size.width),
                                   au::from_px(size.height))
                }
            },
            GenericBox(*) => {
                copy self.d().position
                /* FIXME: The following hits an ICE for whatever reason

                let origin = self.d().position.origin;
                let size   = self.d().position.size;
                let (offset_left, offset_right) = self.get_used_width();
                let (offset_top, offset_bottom) = self.get_used_height();

                Rect {
                    origin: Point2D(origin.x + offset_left, origin.y + offset_top),
                    size:   Size2D(size.width - (offset_left + offset_right),
                                   size.height - (offset_top + offset_bottom))
                }*/
            },
            TextBox(*) => {
                copy self.d().position
            },
            UnscannedTextBox(*) => fail ~"Shouldn't see unscanned boxes here."
        }
    }

    /* The box formed by the border edge, as defined in CSS 2.1 Section 8.1.
       Coordinates are relative to the owning flow. */
    pure fn border_box() -> Rect<au> {
        // TODO: actually compute content_box + padding + border
        self.content_box()
    }

    /* The box fromed by the margin edge, as defined in CSS 2.1 Section 8.1.
       Coordinates are relative to the owning flow. */
    pure fn margin_box() -> Rect<au> {
        // TODO: actually compute content_box + padding + border + margin
        self.content_box()
    }

    // TODO: implement this, generating spacer 
    fn create_inline_spacer_for_side(_ctx: &LayoutContext, _side: InlineSpacerSide) -> Option<@RenderBox> {
        None
    }

    // TODO: to implement stacking contexts correctly, we need to
    // create a set of display lists, one per each layer of a stacking
    // context. (CSS 2.1, Section 9.9.1). Each box is passed the list
    // set representing the box's stacking context. When asked to
    // construct its constituent display items, each box puts its
    // DisplayItems into the correct stack layer (according to CSS 2.1
    // Appendix E).  and then builder flattens the list at the end.

    /* Methods for building a display list. This is a good candidate
       for a function pointer as the number of boxes explodes.

    # Arguments

    * `builder` - the display list builder which manages the coordinate system and options.
    * `dirty` - Dirty rectangle, in the coordinate system of the owning flow (self.ctx)
    * `origin` - Total offset from display list root flow to this box's owning flow
    * `list` - List to which items should be appended
    */
    fn build_display_list(@self, builder: &dl::DisplayListBuilder, dirty: &Rect<au>,
                          offset: &Point2D<au>, list: &dl::DisplayList) {

        let style = self.d().node.style();
        let box_bounds : Rect<au> = match style.position {
            Specified(PosAbsolute) => {
                let x_offset = match style.left {
                    Specified(Px(px)) => au::from_frac_px(px),
                    _ => self.d().position.origin.x
                };
                let y_offset = match style.top {
                    Specified(Px(px)) => au::from_frac_px(px),
                    _ => self.d().position.origin.y
                };
                Rect(Point2D(x_offset, y_offset), copy self.d().position.size)
            }
            _ => {
                self.d().position
            }
        };

        let abs_box_bounds = box_bounds.translate(offset);
        debug!("RenderBox::build_display_list at rel=%?, abs=%?: %s", 
               box_bounds, abs_box_bounds, self.debug_str());
        debug!("RenderBox::build_display_list: dirty=%?, offset=%?", dirty, offset);
        if abs_box_bounds.intersects(dirty) {
            debug!("RenderBox::build_display_list: intersected. Adding display item...");
        } else {
            debug!("RenderBox::build_display_list: Did not intersect...");
            return;
        }

        self.add_bgcolor_to_list(list, &abs_box_bounds); 

        match *self {
            UnscannedTextBox(*) => fail ~"Shouldn't see unscanned boxes here.",
            TextBox(_,d) => {
                list.append_item(~dl::Text(copy abs_box_bounds, text_run::serialize(builder.ctx.font_cache, d.run),
                                           d.offset, d.length))
            },
            // TODO: items for background, border, outline
            GenericBox(_) => {
            },
            ImageBox(_,i) => {
                match i.get_image() {
                    Some(image) => list.append_item(~dl::Image(copy abs_box_bounds, arc::clone(&image))),
                    /* No image data at all? Okay, add some fallback content instead. */
                    None => ()
                }
            }
        }

        self.add_border_to_list(list, abs_box_bounds);
    }

    fn add_bgcolor_to_list(list: &dl::DisplayList, abs_bounds: &Rect<au>) {
        use std::cmp::FuzzyEq;
        // TODO: shouldn't need to unbox CSSValue by now
        let boxed_bgcolor = self.d().node.style().background_color;
        let bgcolor = match boxed_bgcolor {
            Specified(BgColor(c)) => c,
            Specified(BgColorTransparent) | _ => util::color::rgba(0,0,0,0.0)
        };
        if !bgcolor.alpha.fuzzy_eq(&0.0) {
            list.append_item(~dl::SolidColor(copy *abs_bounds, bgcolor.red, bgcolor.green, bgcolor.blue));
        }
    }

    fn add_border_to_list(list: &dl::DisplayList, abs_bounds: Rect<au>) {
        let style = self.d().node.style();
        match style.border_width {
            Specified(Px(px)) => {
                // If there's a border, let's try to display *something*
                let border_width = au::from_frac_px(px);
                let abs_bounds = Rect {
                    origin: Point2D {
                        x: abs_bounds.origin.x - border_width / au(2),
                        y: abs_bounds.origin.y - border_width / au(2),
                    },
                    size: Size2D {
                        width: abs_bounds.size.width + border_width,
                        height: abs_bounds.size.height + border_width
                    }
                };
                let color = match style.border_color {
                    Specified(BdrColor(color)) => color,
                    _ => rgb(0, 0, 0) // FIXME
                };
                list.push(~dl::Border(abs_bounds, border_width, color.red, color.green, color.blue));
            }
            _ => () // TODO
        }
    }
}

impl RenderBox : BoxedDebugMethods {
    fn dump(@self) {
        self.dump_indent(0u);
    }

    /* Dumps the node tree, for debugging, with indentation. */
    fn dump_indent(@self, indent: uint) {
        let mut s = ~"";
        for uint::range(0u, indent) |_i| {
            s += ~"    ";
        }

        s += self.debug_str();
        debug!("%s", s);
    }

    fn debug_str(@self) -> ~str {
        let repr = match self {
            @GenericBox(*) => ~"GenericBox",
            @ImageBox(*) => ~"ImageBox",
            @TextBox(_,d) => fmt!("TextBox(text=%s)", str::substr(d.run.text, d.offset, d.length)),
            @UnscannedTextBox(_,s) => fmt!("UnscannedTextBox(%s)", s)
        };

        fmt!("box b%?: %?", self.d().id, repr)
    }
}
