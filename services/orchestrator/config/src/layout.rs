// Licensed under the Apache-2.0 license
// SPDX-License-Identifier: Apache-2.0

//! Where a device's images live: slots, the golden image, and the byte
//! ranges that hold them.

/// Names one slot in one device's layout. The value is a name, not an
/// index: ids only have to be unique inside one layout, which
/// [`ImageLayout::new`] checks. They do not have to be in order, next to
/// each other, or start at zero, and slot 0 on the BMC has nothing to do
/// with slot 0 on the NIC. Recovery order comes from the order slots are
/// declared in, not from the id values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotId(pub u8);

/// A byte range in the store that holds one device's images. Offsets are
/// counted from the start of that device's area, so the platform driver is
/// the one that knows which part holds it and where the area starts.
///
/// Base and length describe any store the eRoT addresses directly. That is
/// flash today, and an EEPROM or a memory-mapped part would have the same
/// shape. An image with no address, streamed or held inside a self-updating
/// device, is not described this way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    base: u32,
    len: u32,
}

impl Region {
    /// Declares a region. Const, so a bad board table fails the build
    /// instead of the boot.
    ///
    /// # Panics
    ///
    /// Panics if `len` is zero, or if `base + len` would run past the end
    /// of the 32-bit offset range.
    #[must_use]
    pub const fn new(base: u32, len: u32) -> Self {
        assert!(len > 0, "region length must not be zero");
        assert!(
            base.checked_add(len).is_some(),
            "region must not run past the end of the offset space"
        );
        Self { base, len }
    }

    /// Offset of the first byte, from the start of the device's firmware
    /// partition.
    #[must_use]
    pub const fn base(&self) -> u32 {
        self.base
    }

    /// Length in bytes: how much the region holds, not the size of the
    /// image now in it.
    #[must_use]
    pub const fn len(&self) -> u32 {
        self.len
    }

    /// Whether the region holds no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Offset one past the last byte.
    #[must_use]
    pub const fn end(&self) -> u32 {
        self.base + self.len
    }

    /// Whether the two regions share a byte.
    pub(crate) const fn overlaps(&self, other: &Region) -> bool {
        self.base < other.end() && other.base < self.end()
    }
}

/// One slot in a device's layout. The number of slots is data, so a layout
/// is A/B, a single slot, or any other count depending on what the board
/// declares, and no layout shape is named in code.
///
/// Every slot can be written and booted. The golden image can be neither,
/// and it is not a slot, so there is no flag to set and nothing to check.
#[derive(Debug, Clone, Copy)]
pub struct Slot {
    id: SlotId,
    region: Region,
}

impl Slot {
    /// Declares one slot. Const, so board tables still build at compile
    /// time. Rules that cover a whole layout (unique ids, no overlap) are
    /// checked by [`ImageLayout::new`], which sees the whole list.
    #[must_use]
    pub const fn new(id: SlotId, region: Region) -> Self {
        Self { id, region }
    }

    /// This slot's id, unique within the layout (checked by
    /// [`ImageLayout::new`]).
    #[must_use]
    pub const fn id(&self) -> SlotId {
        self.id
    }

    /// Where this slot lives in the device's image store.
    #[must_use]
    pub const fn region(&self) -> Region {
        self.region
    }
}

/// The golden image: the last image recovery falls back to, and the one
/// image the eRoT never writes.
///
/// It is not a slot, so code that walks the slot list to pick a write
/// target, or to check the SVN floor, never sees it. The floor has to skip
/// it. A golden image's security version is fixed when the board is made,
/// so once the floor moves past that version, checking the floor would make
/// the last image that still boots unbootable.
///
/// Skipping the floor is only safe while the image really cannot be
/// written, which the board has to guarantee in hardware: a write-protect
/// pin, a locked flash block, or a separate part. This crate cannot check
/// that.
#[derive(Debug, Clone, Copy)]
pub struct Golden {
    region: Region,
}

impl Golden {
    /// Declares a layout's golden image.
    #[must_use]
    pub const fn new(region: Region) -> Self {
        Self { region }
    }

    /// Where the golden image lives in the device's image store.
    #[must_use]
    pub const fn region(&self) -> Region {
        self.region
    }
}

/// Every image of one device that the eRoT can address: the slots it
/// writes, plus the golden image when there is one. Slots are declared in
/// recovery order: recovery tries them from top to bottom and falls back to
/// the golden image last. A board with plain A/B slots carries no golden
/// image, and recovery then stops after the last slot.
///
/// A device has a layout when the eRoT owns its flash and writes its
/// images. A device that takes its own updates over PLDM and picks what it
/// boots has no layout, and the eRoT never names a byte range for it. That
/// is all [`DeviceConfig::layout`] being an `Option` says; how the eRoT
/// talks to such a device is decided elsewhere.
#[derive(Debug, Clone, Copy)]
pub struct ImageLayout {
    slots: &'static [Slot],
    golden: Option<Golden>,
}

impl ImageLayout {
    /// Declares a device's images. Const, so a bad layout fails the
    /// build.
    ///
    /// # Panics
    ///
    /// Panics if the layout holds no image, if two slots share an id, or
    /// if two regions overlap, the golden image included. Overlapping
    /// regions would let an update to one image corrupt another.
    ///
    /// The overlap check only covers addresses. Two regions that share a
    /// flash erase block still wipe each other even though they do not
    /// overlap, and this crate does not know the erase block size, so a
    /// board checks that in its own const fence.
    #[must_use]
    pub const fn new(slots: &'static [Slot], golden: Option<Golden>) -> Self {
        assert!(
            !slots.is_empty() || golden.is_some(),
            "a layout must hold at least one image"
        );
        let mut s = 0;
        while s < slots.len() {
            if let Some(golden) = golden {
                assert!(
                    !slots[s].region.overlaps(&golden.region),
                    "a slot must not overlap the golden image"
                );
            }
            let mut t = s + 1;
            while t < slots.len() {
                assert!(
                    slots[s].id.0 != slots[t].id.0,
                    "slot ids must be unique within a layout"
                );
                assert!(
                    !slots[s].region.overlaps(&slots[t].region),
                    "slots must not overlap"
                );
                t += 1;
            }
            s += 1;
        }
        Self { slots, golden }
    }

    /// The slots the eRoT writes, in declaration order. Empty when the
    /// golden image is the device's only image.
    #[must_use]
    pub const fn slots(&self) -> &'static [Slot] {
        self.slots
    }

    /// The golden image, when the board has one.
    #[must_use]
    pub const fn golden(&self) -> Option<Golden> {
        self.golden
    }

    /// How many images recovery can try: every slot, then the golden image
    /// if there is one.
    #[must_use]
    pub const fn image_count(&self) -> usize {
        self.slots.len() + self.golden.is_some() as usize
    }

    /// The image recovery attempt `attempt` uses, or `None` once every
    /// image has had its attempt.
    ///
    /// `attempt` is the count `Recovery::restore` receives, `0` on the first
    /// try of a recovery cycle. Attempt `n` names slot `n` in declaration
    /// order, and the attempt after the last slot names the golden image.
    /// A board answers `SourceExhausted` for `None`, so the state machine
    /// treats the component per its failure policy without waiting for the
    /// retry cap.
    ///
    /// [`assert_retry_reaches_every_image`](crate::assert_retry_reaches_every_image)
    /// compares the retry cap with [`image_count`](Self::image_count), and
    /// `image_for_attempt` returns `None` from attempt `image_count` on.
    #[must_use]
    pub const fn image_for_attempt(&self, attempt: u8) -> Option<LayoutImage> {
        let attempt = attempt as usize;
        if attempt < self.slots.len() {
            return Some(LayoutImage::Slot(self.slots[attempt]));
        }
        match self.golden {
            Some(golden) if attempt == self.slots.len() => Some(LayoutImage::Golden(golden)),
            _ => None,
        }
    }
}

/// One image of a layout, as [`ImageLayout::image_for_attempt`] names it.
///
/// `Slot` and `Golden` stay apart because a board may write a slot and must
/// not write the golden image.
#[derive(Debug, Clone, Copy)]
pub enum LayoutImage {
    /// A slot of the layout.
    Slot(Slot),
    /// The golden image, the last image of a recovery cycle.
    Golden(Golden),
}

impl LayoutImage {
    /// Where the image sits in the image store of the device.
    #[must_use]
    pub const fn region(&self) -> Region {
        match self {
            Self::Slot(slot) => slot.region(),
            Self::Golden(golden) => golden.region(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Board tables run the constructors at compile time, where a
    // rejection is a build error nobody can assert on. These tests call
    // them at runtime to prove the reject paths actually fire.

    /// Slots are one megabyte each; the values only have to be distinct
    /// and non-overlapping.
    const SLOT_LEN: u32 = 0x10_0000;

    /// An ordinary slot, placed by id so two of them never overlap by
    /// accident.
    const fn slot(id: u8) -> Slot {
        Slot::new(SlotId(id), Region::new(id as u32 * SLOT_LEN, SLOT_LEN))
    }

    /// The golden image, above every slot `slot` can place.
    const GOLDEN: Golden = Golden::new(Region::new(0xF000_0000, SLOT_LEN));

    const LAYOUT: ImageLayout = ImageLayout::new(const { &[slot(0), slot(1)] }, Some(GOLDEN));

    /// The golden image may be a device's only image.
    #[test]
    fn accepts_a_layout_with_no_slots() {
        let layout = ImageLayout::new(&[], Some(GOLDEN));
        assert!(layout.slots().is_empty());
    }

    #[test]
    #[should_panic(expected = "slot ids must be unique")]
    fn rejects_duplicate_slot_ids() {
        let _ = ImageLayout::new(
            const {
                &[
                    Slot::new(SlotId(0), Region::new(0, SLOT_LEN)),
                    Slot::new(SlotId(0), Region::new(SLOT_LEN, SLOT_LEN)),
                ]
            },
            Some(GOLDEN),
        );
    }

    #[test]
    #[should_panic(expected = "slots must not overlap")]
    fn rejects_overlapping_slots() {
        let _ = ImageLayout::new(
            const {
                &[
                    Slot::new(SlotId(0), Region::new(0, SLOT_LEN)),
                    Slot::new(SlotId(1), Region::new(SLOT_LEN - 1, SLOT_LEN)),
                ]
            },
            Some(GOLDEN),
        );
    }

    #[test]
    #[should_panic(expected = "must not overlap the golden image")]
    fn rejects_a_slot_overlapping_the_golden_image() {
        let _ = ImageLayout::new(
            const { &[Slot::new(SlotId(0), Region::new(0xF000_0000, SLOT_LEN))] },
            Some(GOLDEN),
        );
    }

    /// Regions that touch do not overlap: `end` is one past the last
    /// byte.
    #[test]
    fn accepts_adjacent_regions() {
        let layout = ImageLayout::new(
            const {
                &[
                    Slot::new(SlotId(0), Region::new(0, SLOT_LEN)),
                    Slot::new(SlotId(1), Region::new(SLOT_LEN, SLOT_LEN)),
                ]
            },
            Some(GOLDEN),
        );
        assert_eq!(
            layout.slots()[0].region().end(),
            layout.slots()[1].region().base()
        );
    }

    /// What `image_for_attempt` returns for an attempt, by slot id.
    #[derive(Debug, PartialEq)]
    enum Named {
        Slot(u8),
        Golden,
        Exhausted,
    }

    fn named(layout: &ImageLayout, attempt: u8) -> Named {
        match layout.image_for_attempt(attempt) {
            Some(LayoutImage::Slot(slot)) => Named::Slot(slot.id().0),
            Some(LayoutImage::Golden(_)) => Named::Golden,
            None => Named::Exhausted,
        }
    }

    #[test]
    fn recovery_tries_every_slot_in_order_and_the_golden_image_last() {
        assert_eq!(named(&LAYOUT, 0), Named::Slot(0));
        assert_eq!(named(&LAYOUT, 1), Named::Slot(1));
        assert_eq!(named(&LAYOUT, 2), Named::Golden);
        assert_eq!(named(&LAYOUT, 3), Named::Exhausted);
        assert_eq!(named(&LAYOUT, u8::MAX), Named::Exhausted);
    }

    #[test]
    fn recovery_stops_after_the_last_slot_without_a_golden_image() {
        let layout = ImageLayout::new(const { &[slot(0), slot(1)] }, None);
        assert_eq!(named(&layout, 1), Named::Slot(1));
        assert_eq!(named(&layout, 2), Named::Exhausted);
    }

    #[test]
    fn recovery_starts_at_the_golden_image_when_the_layout_has_no_slot() {
        let layout = ImageLayout::new(&[], Some(GOLDEN));
        assert_eq!(named(&layout, 0), Named::Golden);
        assert_eq!(named(&layout, 1), Named::Exhausted);
    }

    #[test]
    fn the_golden_attempt_names_the_golden_region() {
        // The attempt after the last slot.
        let golden_attempt = LAYOUT.slots().len() as u8;
        let image = LAYOUT.image_for_attempt(golden_attempt).unwrap();
        assert_eq!(image.region().base(), GOLDEN.region().base());
        assert_eq!(image.region().len(), GOLDEN.region().len());
    }

    /// The retry budget assert counts images with `image_count`, so the
    /// attempts that name an image have to be exactly the attempts below it.
    #[test]
    fn an_attempt_names_an_image_exactly_when_it_is_below_the_image_count() {
        for layout in [
            LAYOUT,
            ImageLayout::new(const { &[slot(0), slot(1)] }, None),
            ImageLayout::new(&[], Some(GOLDEN)),
        ] {
            for attempt in 0..=u8::MAX {
                assert_eq!(
                    layout.image_for_attempt(attempt).is_some(),
                    usize::from(attempt) < layout.image_count(),
                    "attempt {attempt}"
                );
            }
        }
    }

    /// A board with plain A/B slots carries no golden image.
    #[test]
    fn accepts_slots_without_a_golden_image() {
        let layout = ImageLayout::new(const { &[slot(0), slot(1)] }, None);
        assert!(layout.golden().is_none());
        assert_eq!(layout.image_count(), 2);
    }

    #[test]
    #[should_panic(expected = "at least one image")]
    fn rejects_a_layout_with_no_image() {
        let _ = ImageLayout::new(&[], None);
    }

    #[test]
    fn counts_every_slot_and_the_golden_image() {
        assert_eq!(LAYOUT.image_count(), 3);
        assert_eq!(ImageLayout::new(&[], Some(GOLDEN)).image_count(), 1);
    }

    #[test]
    #[should_panic(expected = "region length must not be zero")]
    fn rejects_a_zero_length_region() {
        let _ = Region::new(0, 0);
    }

    #[test]
    #[should_panic(expected = "must not run past the end of the offset space")]
    fn rejects_a_region_past_the_end_of_the_offset_space() {
        let _ = Region::new(u32::MAX, 1);
    }
}
