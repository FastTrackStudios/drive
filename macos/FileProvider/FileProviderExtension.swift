// The Drive tree in Finder (docs/fileprovider.md).
//
// The system loads this from the app bundle and asks it for the tree,
// through `NSFileProviderReplicatedExtension`. Every answer comes from the
// running agent (Bridge.swift): this keeps no state of its own and touches
// no file outside the agent's folder (~/.local/share/drive-agent), where the
// agent's socket is and where bytes are staged both ways.

import FileProvider
import Foundation
import os

private let log = Logger(subsystem: "app.fasttrackstudio.drive", category: "fileprovider")

@objc(FileProviderExtension)
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    private let domain: NSFileProviderDomain
    private var watch: DispatchSourceTimer?
    private var seen: String?

    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        super.init()
        // Nothing may block here: the system kills a launch that takes
        // long. The watch starts on a queue of its own.
        startWatching()
    }

    func invalidate() {
        watch?.cancel()
        watch = nil
    }

    /// Tell the system when the tree changed elsewhere (another device,
    /// the mount, an app), so Finder asks for the changes. A cheap poll of
    /// the tree's version while the extension is loaded.
    private func startWatching() {
        let t = DispatchSource.makeTimerSource(queue: .global(qos: .utility))
        t.schedule(deadline: .now() + 2, repeating: 3)
        t.setEventHandler { [weak self] in
            guard let self, let now = try? Agent.anchor() else { return }
            if let seen = self.seen, seen != now {
                NSFileProviderManager(for: self.domain)?.signalEnumerator(for: .workingSet) { error in
                    if let error {
                        log.error("signal: \(error.localizedDescription, privacy: .public)")
                    }
                }
            }
            self.seen = now
        }
        t.resume()
        watch = t
    }

    private func fail(_ error: Error) -> Error { providerError(error) }

    // ── Reading ──────────────────────────────────────────────────────

    func item(for identifier: NSFileProviderItemIdentifier,
              request _: NSFileProviderRequest,
              completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        do {
            completionHandler(Item(try Agent.item(driveID(identifier))), nil)
        } catch {
            completionHandler(nil, fail(error))
        }
        progress.completedUnitCount = 1
        return progress
    }

    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier,
                    request _: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        Enumerator(container: containerItemIdentifier)
    }

    /// Something wants a file's bytes: the agent brings them here (from
    /// its cache, or another device through the node) into the group
    /// container, and the system takes that copy.
    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier,
                       version _: NSFileProviderItemVersion?,
                       request _: NSFileProviderRequest,
                       completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 100)
        let id = driveID(itemIdentifier)
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let got = try Agent.fetch(id)
                progress.completedUnitCount = 100
                completionHandler(URL(fileURLWithPath: got.path), Item(got.item), nil)
            } catch {
                log.error("fetch \(id, privacy: .public): \(error.localizedDescription, privacy: .public)")
                completionHandler(nil, nil, self.fail(error))
            }
        }
        return progress
    }

    // ── Writing: what the mount does, through the agent ──────────────

    func createItem(basedOn template: NSFileProviderItem,
                    fields _: NSFileProviderItemFields,
                    contents url: URL?,
                    options _: NSFileProviderCreateItemOptions = [],
                    request _: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void)
        -> Progress {
        let progress = Progress(totalUnitCount: 1)
        DispatchQueue.global(qos: .userInitiated).async {
            var staged: URL?
            defer { staged.map { try? FileManager.default.removeItem(at: $0) } }
            do {
                let dir = template.contentType == .folder
                if !dir, let url { staged = try Agent.stage(url) }
                let made = try Agent.create(parent: driveID(template.parentItemIdentifier),
                                            name: template.filename, dir: dir, from: staged?.path)
                progress.completedUnitCount = 1
                completionHandler(Item(made), [], false, nil)
            } catch {
                completionHandler(nil, [], false, self.fail(error))
            }
        }
        return progress
    }

    func modifyItem(_ item: NSFileProviderItem,
                    baseVersion _: NSFileProviderItemVersion,
                    changedFields: NSFileProviderItemFields,
                    contents url: URL?,
                    options _: NSFileProviderModifyItemOptions = [],
                    request _: NSFileProviderRequest,
                    completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void)
        -> Progress {
        let progress = Progress(totalUnitCount: 1)
        DispatchQueue.global(qos: .userInitiated).async {
            var staged: URL?
            defer { staged.map { try? FileManager.default.removeItem(at: $0) } }
            do {
                if changedFields.contains(.contents), let url { staged = try Agent.stage(url) }
                let renamed = changedFields.contains(.filename) ? item.filename : nil
                let moved = changedFields.contains(.parentItemIdentifier)
                    ? driveID(item.parentItemIdentifier) : nil
                let now = try Agent.modify(driveID(item.itemIdentifier), name: renamed,
                                           parent: moved, from: staged?.path)
                progress.completedUnitCount = 1
                // Nothing is left pending: fields this provider does not
                // keep (tags, dates set by hand) are taken as done.
                completionHandler(Item(now), [], false, nil)
            } catch {
                completionHandler(nil, [], false, self.fail(error))
            }
        }
        return progress
    }

    func deleteItem(identifier: NSFileProviderItemIdentifier,
                    baseVersion _: NSFileProviderItemVersion,
                    options _: NSFileProviderDeleteItemOptions = [],
                    request _: NSFileProviderRequest,
                    completionHandler: @escaping (Error?) -> Void) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        do {
            // Into the Drive's Trash: nothing is ever deleted.
            try Agent.delete(driveID(identifier))
            completionHandler(nil)
        } catch {
            completionHandler(fail(error))
        }
        progress.completedUnitCount = 1
        return progress
    }
}
