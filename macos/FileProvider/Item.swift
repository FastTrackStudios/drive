// One item of the Drive tree, as the File Provider system wants it.
//
// Identifiers are the tree's own ids, which survive renames and moves
// (Task's are paths, because its tree is files on disk; Drive's is a
// document whose nodes keep their id wherever they go). The top is the
// root container; the Trash is never an item.

import FileProvider
import UniformTypeIdentifiers

let rootID = "root"

/// Bumped whenever what this file reports about an item changes (its
/// capabilities, its type): it is part of every metadata version, so the
/// system asks again about items it already knows. Without it, items keep
/// what an older extension said (as the first build's missing
/// `.allowsEvicting` did).
let itemSchema = 2

func identifier(_ id: String) -> NSFileProviderItemIdentifier {
    id == rootID ? .rootContainer : NSFileProviderItemIdentifier(id)
}

func driveID(_ identifier: NSFileProviderItemIdentifier) -> String {
    identifier == .rootContainer ? rootID : identifier.rawValue
}

final class Item: NSObject, NSFileProviderItem {
    let drive: DriveItem

    init(_ drive: DriveItem) {
        self.drive = drive
    }

    var itemIdentifier: NSFileProviderItemIdentifier { identifier(drive.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier { identifier(drive.parent) }
    var filename: String { drive.id == rootID ? "Drive" : drive.name }

    var contentType: UTType {
        if drive.dir { return .folder }
        return UTType(filenameExtension: (drive.name as NSString).pathExtension) ?? .data
    }

    /// The file's real size, whether or not its bytes are here.
    var documentSize: NSNumber? { drive.dir ? nil : NSNumber(value: drive.size) }

    var contentModificationDate: Date? {
        drive.mtime > 0 ? Date(timeIntervalSince1970: TimeInterval(drive.mtime) / 1000) : nil
    }

    /// `.allowsEvicting` is what lets a file's bytes be given back (Remove
    /// Download, or the system when space is short); without it the system
    /// refuses with -2008 and keeps every byte ever read.
    var capabilities: NSFileProviderItemCapabilities {
        drive.dir
            ? [.allowsReading, .allowsContentEnumerating, .allowsAddingSubItems,
               .allowsRenaming, .allowsReparenting, .allowsDeleting, .allowsEvicting]
            : [.allowsReading, .allowsWriting, .allowsRenaming, .allowsReparenting,
               .allowsDeleting, .allowsEvicting]
    }

    /// The content version changes with the bytes, the metadata version
    /// with the name or place: the system re-downloads only for the first.
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data(drive.content.utf8),
                                  metadataVersion: Data("\(itemSchema)|\(drive.meta)".utf8))
    }

    /// Bytes on this device (no cloud badge): the agent's cache holds them.
    var isDownloaded: Bool { drive.here }
    var isMostRecentVersionDownloaded: Bool { drive.here }
    var isUploaded: Bool { true }
}
