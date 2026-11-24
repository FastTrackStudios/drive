// Listing a folder, and saying what changed.
//
// A folder lists a page at a time (`fp_list`). Changes come from the tree
// document's version (`fp_anchor`, `fp_changes`): the agent compares the
// tree at an anchor with the tree now, so this keeps no journal of its
// own. Changes are reported for the whole tree on the working set (the
// replicated API's way to hear about everything); a folder's own
// enumerator reports those under it.
//
// An anchor is "expired" only when the agent says so: answering
// `syncAnchorExpired` when nothing changed makes the system enumerate
// again forever (Task's first extension found this the hard way).

import FileProvider
import Foundation

final class Enumerator: NSObject, NSFileProviderEnumerator {
    /// The folder listed; nil for the working set.
    private let container: String?

    init(container: NSFileProviderItemIdentifier) {
        self.container = container == .workingSet ? nil : driveID(container)
    }

    func invalidate() {}

    func enumerateItems(for observer: NSFileProviderEnumerationObserver,
                        startingAt page: NSFileProviderPage) {
        // The working set's items are what the folders already listed:
        // its changes are what matter (enumerateChanges).
        guard let container else {
            observer.finishEnumerating(upTo: nil)
            return
        }
        let start = page.rawValue
        let after: String? =
            start == NSFileProviderPage.initialPageSortedByName as Data
            || start == NSFileProviderPage.initialPageSortedByDate as Data
            ? nil : String(data: start, encoding: .utf8)
        do {
            let got = try Agent.list(container, after: after)
            observer.didEnumerate(got.items.map(Item.init))
            observer.finishEnumerating(upTo: got.next.map { NSFileProviderPage(Data($0.utf8)) })
        } catch {
            observer.finishEnumeratingWithError(providerError(error))
        }
    }

    func enumerateChanges(for observer: NSFileProviderChangeObserver,
                          from anchor: NSFileProviderSyncAnchor) {
        do {
            let since = String(data: anchor.rawValue, encoding: .utf8) ?? ""
            let c = try Agent.changes(since: since)
            if c.expired == true {
                observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
                return
            }
            let updated = container.map { folder in c.updated.filter { $0.parent == folder } }
                ?? c.updated
            if !updated.isEmpty { observer.didUpdate(updated.map(Item.init)) }
            if !c.deleted.isEmpty { observer.didDeleteItems(withIdentifiers: c.deleted.map(identifier)) }
            observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(Data(c.anchor.utf8)),
                                              moreComing: false)
        } catch {
            observer.finishEnumeratingWithError(providerError(error))
        }
    }

    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler((try? Agent.anchor()).map { NSFileProviderSyncAnchor(Data($0.utf8)) })
    }
}
