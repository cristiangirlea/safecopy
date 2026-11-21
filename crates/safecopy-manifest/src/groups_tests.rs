use super::*;
use crate::groups::parent;

const MIB: u64 = 1 << 20;

fn file(path: &str, size: u64) -> Entry {
    Entry {
        path: path.to_string(),
        kind: Kind::File,
        size,
    }
}

fn folder(path: &str) -> Entry {
    Entry {
        path: path.to_string(),
        kind: Kind::Folder,
        size: 0,
    }
}

/// Every place in exactly one group, in order, none empty.
fn assert_covers(groups: &Groups, entries: usize) {
    let mut next = 0;
    for group in groups.groups() {
        assert_eq!(group.first, next, "groups follow one another");
        assert!(group.len > 0);
        next = group.end();
    }
    assert_eq!(next as usize, entries);
}

#[test]
fn a_flat_folder_of_10000_entries_is_cut_into_groups_of_at_most_256_entries_or_256_mib() {
    let limits = GroupLimits::default();
    let small: Vec<Entry> = (0..10_000)
        .map(|n| file(&format!("Flat/{n:05}.txt"), 1000))
        .collect();
    let cut = groups(&small, &limits);
    assert_covers(&cut, 10_000);
    assert_eq!(cut.groups().len(), 40, "10,000 / 256, rounded up");
    assert!(cut.groups().iter().all(|g| g.len <= 256));

    // Files of 3 MiB: 256 MiB comes before 256 entries.
    let middling: Vec<Entry> = (0..10_000)
        .map(|n| file(&format!("Flat/{n:05}.bin"), 3 * MIB))
        .collect();
    let cut = groups(&middling, &limits);
    assert_covers(&cut, 10_000);
    for group in cut.groups() {
        assert!(group.bytes <= 256 * MIB);
        assert!(group.len <= 256);
    }
    assert_eq!(cut.groups()[0].len, 85, "85 x 3 MiB = 255 MiB");

    // A file over 256 MiB is a group of its own, wherever it falls.
    let mut mixed: Vec<Entry> = (0..300)
        .map(|n| file(&format!("Flat/{n:03}"), 10))
        .collect();
    mixed.insert(100, file("Flat/huge", 300 * MIB));
    let cut = groups(&mixed, &limits);
    assert_covers(&cut, 301);
    let huge = cut.of(100).unwrap();
    assert_eq!(
        cut.get(huge).copied(),
        Some(Group {
            first: 100,
            len: 1,
            bytes: 300 * MIB
        })
    );
}

#[test]
fn a_nested_tree_is_cut_where_the_parent_changes_and_small_folders_share_a_group() {
    let limits = GroupLimits::default();
    let check = |entries: &[Entry]| {
        let cut = groups(entries, &limits);
        assert_covers(&cut, entries.len());
        let (last, rest) = cut.groups().split_last().unwrap();
        for group in rest {
            assert!(
                (64..=256).contains(&group.len),
                "a group of {} entries",
                group.len
            );
        }
        assert!(last.len <= 256);
        // Each cut falls where the parent changes.
        for group in rest {
            let end = group.end() as usize;
            assert_ne!(parent(&entries[end - 1].path), parent(&entries[end].path));
        }
        cut
    };

    // As a paste's list: 5,000 folders of one file each, the folders entries too.
    let paste: Vec<Entry> = (0..5000)
        .flat_map(|n| {
            [
                folder(&format!("Top/d{n:04}")),
                file(&format!("Top/d{n:04}/f.txt"), 100),
            ]
        })
        .collect();
    let cut = check(&paste);
    assert!(cut.groups().len() < 200, "not 5,000 groups of one");

    // As a folder's listing: its files only, the folder's name first.
    let listing: Vec<Entry> = (0..5000)
        .map(|n| file(&format!("Top/d{n:04}/f.txt"), 100))
        .collect();
    check(&listing);

    // A paste's name order: a subfolder between two runs of its parent's files.
    let mut order = vec![folder("A")];
    order.extend((0..70).map(|n| file(&format!("A/a{n:02}"), 1)));
    order.push(folder("A/m"));
    order.extend((0..70).map(|n| file(&format!("A/m/x{n:02}"), 1)));
    order.extend((0..70).map(|n| file(&format!("A/z{n:02}"), 1)));
    let cut = groups(&order, &limits);
    assert_covers(&cut, order.len());
    let firsts: Vec<u32> = cut.groups().iter().map(|g| g.first).collect();
    // "A", its first run and the folder "A/m" (whose parent is "A"), then
    // what "A/m" holds, then the second run of "A".
    assert_eq!(firsts, vec![0, 72, 142]);
}

/// A shuffle of `0..n` from a fixed seed.
fn shuffled(n: u32, mut seed: u64) -> Vec<u32> {
    let mut places: Vec<u32> = (0..n).collect();
    for i in (1..places.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        places.swap(i, (seed % (i as u64 + 1)) as usize);
    }
    places
}

#[test]
fn the_mark_moves_only_when_the_oldest_open_group_is_settled() {
    let limits = GroupLimits {
        entries: 16,
        min_entries: 16,
        ..GroupLimits::default()
    };
    let list: Vec<Entry> = (0..64).map(|n| file(&format!("F/{n:02}"), 1)).collect();
    let cut = groups(&list, &limits);
    assert_eq!(cut.groups().len(), 4);
    let mut progress = Progress::new(&limits, 134);
    cut.open(0, &mut progress).unwrap();
    cut.open(1, &mut progress).unwrap();
    // A third waits for the oldest to finish.
    assert_eq!(cut.open(2, &mut progress), Err(ProgressError::TooManyOpen));

    // Both open groups' entries settled in a shuffled order.
    for (n, offset) in shuffled(32, 0x5eed).into_iter().enumerate() {
        let before = progress.mark();
        let oldest = progress.open_groups()[0].clone();
        let completes =
            (oldest.first..oldest.first + oldest.len).all(|p| p == offset || oldest.is_settled(p));
        let moved = progress.settle(offset).unwrap();
        if completes {
            assert!(moved >= 1, "after {n} settled");
        } else {
            assert_eq!(moved, 0, "the mark stays until the oldest group is whole");
            assert_eq!(progress.mark(), before);
        }
        // The settled entries past the mark are exactly the open groups' bits.
        for place in progress.below()..32 {
            let in_bits = progress.open_groups().iter().any(|g| g.is_settled(place));
            assert_eq!(progress.skips(place), in_bits);
        }
        assert!(progress.open_groups().len() <= 2);
    }
    assert_eq!(progress.mark(), 2);
    assert_eq!(progress.below(), 32);
    assert!(progress.open_groups().is_empty());
    // A settled entry is not settled twice, and the next group opens.
    assert_eq!(progress.settle(3), Err(ProgressError::Settled));
    assert_eq!(progress.settle(40), Err(ProgressError::NotOpen));
    cut.open(2, &mut progress).unwrap();
}

#[test]
fn a_slow_file_holds_up_new_work_by_at_most_one_group() {
    let limits = GroupLimits {
        entries: 16,
        min_entries: 16,
        ..GroupLimits::default()
    };
    let list: Vec<Entry> = (0..64).map(|n| file(&format!("F/{n:02}"), 1)).collect();
    let cut = groups(&list, &limits);
    let mut progress = Progress::new(&limits, 8);
    // The last file of group 1 (the second group) is held by the gate.
    let slow = cut.get(1).unwrap().end() - 1;
    let mut gate_open = false;
    let mut writing: Vec<u32> = Vec::new();
    let mut log = Vec::new();

    for _round in 0..200 {
        // Start whatever may start, up to 8 at once, in the list's order.
        for place in 0..64 {
            if writing.len() >= 8 {
                break;
            }
            if !cut.may_start(place, &progress) {
                continue;
            }
            let index = cut.of(place).unwrap();
            if !progress.open_groups().iter().any(|g| g.index == index) {
                cut.open(index, &mut progress).unwrap();
                log.push(("open", index, gate_open));
            }
            progress.start(place).unwrap();
            writing.push(place);
        }
        // Everything but the slow file finishes.
        let finished: Vec<u32> = writing
            .iter()
            .copied()
            .filter(|p| *p != slow || gate_open)
            .collect();
        for place in finished {
            progress.settle(place).unwrap();
            writing.retain(|p| *p != place);
        }
        let group2_whole =
            (cut.get(2).unwrap().first..cut.get(2).unwrap().end()).all(|p| progress.skips(p));
        if group2_whole && !gate_open {
            // Group 3 waits for the slow file while it is held.
            assert!(!cut.may_start(cut.get(3).unwrap().first, &progress));
            assert!(progress.open_groups().len() <= 2);
            gate_open = true;
        }
        if progress.mark() == 4 {
            break;
        }
    }
    assert_eq!(progress.mark(), 4, "everything was settled");
    // Group 2 opened while the slow file was held; group 3 only after.
    assert!(log.contains(&("open", 2, false)));
    assert!(log.contains(&("open", 3, true)));
    assert!(!log.contains(&("open", 3, false)));
}

#[test]
fn a_progress_read_back_must_be_one_the_calls_could_make() {
    let limits = GroupLimits::default();
    let ok = Progress::restore(1, 10, &[(1, 10, 3, vec![0b010])], &[10], &limits, 8).unwrap();
    assert!(ok.skips(5) && ok.skips(11) && !ok.skips(12));
    assert!(ok.writing(10));
    // A bit past the group, a whole oldest group, a group out of order, too many open.
    assert!(Progress::restore(1, 10, &[(1, 10, 3, vec![0b1000])], &[], &limits, 8).is_err());
    assert!(Progress::restore(1, 10, &[(1, 10, 3, vec![0b111])], &[], &limits, 8).is_err());
    assert!(Progress::restore(1, 10, &[(2, 10, 3, vec![0])], &[], &limits, 8).is_err());
    let three = [
        (1, 10, 1, vec![0]),
        (2, 11, 1, vec![0]),
        (3, 12, 1, vec![0]),
    ];
    assert!(Progress::restore(1, 10, &three, &[], &limits, 8).is_err());
    // A partial settled, or in no open group.
    assert!(Progress::restore(1, 10, &[(1, 10, 3, vec![0b010])], &[11], &limits, 8).is_err());
    assert!(Progress::restore(1, 10, &[], &[30], &limits, 8).is_err());
}
