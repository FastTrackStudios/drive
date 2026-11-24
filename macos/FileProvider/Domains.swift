// Registering the Drive's File Provider domain.
//
// Only the containing app may add a domain, so this is a small tool in the
// app bundle that the app runs (or a person, from a terminal):
//
//   DriveFileProviderDomains sync     add the Drive domain (idempotent)
//   DriveFileProviderDomains list     what is registered
//   DriveFileProviderDomains clear    remove it
//   DriveFileProviderDomains signal   ask Finder to fetch the changes now
//   DriveFileProviderDomains watch    signal whenever the tree changes (runs on)
//   DriveFileProviderDomains evict P  give back a file's bytes (Remove Download)
//   DriveFileProviderDomains ping     can the agent be reached
//
// `watch` is what keeps Finder current with changes made elsewhere: the
// extension only runs while the system needs it (it is relaunched every
// minute or so), so a poll inside it misses changes. The menu-bar app
// will run this; until then a person or launchd does.

import FileProvider
import Foundation

@main
enum Domains {
    static let drive = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier("drive"),
                                            displayName: "Drive")

    static func main() async {
        let verb = CommandLine.arguments.dropFirst().first ?? "sync"
        do {
            switch verb {
            case "sync":
                let have = try await NSFileProviderManager.domains()
                if have.contains(where: { $0.identifier == drive.identifier }) {
                    print("Drive is registered")
                } else {
                    try await NSFileProviderManager.add(drive)
                    print("added Drive")
                }
            case "list":
                for d in try await NSFileProviderManager.domains() {
                    print("\(d.identifier.rawValue)\t\(d.displayName)")
                }
            case "clear":
                try await NSFileProviderManager.removeAllDomains()
                print("removed every domain of this app")
            case "signal":
                guard let m = NSFileProviderManager(for: drive) else { throw Plain("no Drive domain") }
                try await m.signalEnumerator(for: .workingSet)
                print("signalled")
            case "watch":
                guard let m = NSFileProviderManager(for: drive) else { throw Plain("no Drive domain") }
                var seen: String?
                print("watching the Drive tree (ctrl-c to stop)")
                while true {
                    if let now = try? Agent.anchor() {
                        if let was = seen, was != now {
                            try? await m.signalEnumerator(for: .workingSet)
                        }
                        seen = now
                    }
                    try await Task.sleep(nanoseconds: 2_000_000_000)
                }
            case "evict":
                guard let path = CommandLine.arguments.dropFirst(2).first else { throw Plain("evict PATH") }
                guard let m = NSFileProviderManager(for: drive) else { throw Plain("no Drive domain") }
                let url = URL(fileURLWithPath: path)
                let id: NSFileProviderItemIdentifier = try await withCheckedThrowingContinuation { k in
                    NSFileProviderManager.getIdentifierForUserVisibleFile(at: url) { item, _, error in
                        if let item { k.resume(returning: item) } else {
                            k.resume(throwing: error ?? Plain("\(path) is not in the Drive folder"))
                        }
                    }
                }
                try await m.evictItem(identifier: id)
                print("evicted \(path)")
            case "ping":
                print("agent at tree version \(try Agent.anchor())")
                let top = try Agent.list(rootID, after: nil)
                print("top: \(top.items.map(\.name).joined(separator: ", "))")
            default:
                throw Plain("usage: DriveFileProviderDomains sync|list|clear|signal|ping")
            }
        } catch {
            // The domain and code too: macOS answers several different
            // refusals with the same sentence, and the code tells them apart.
            let ns = error as NSError
            FileHandle.standardError.write(Data("\(ns.localizedDescription) [\(ns.domain) \(ns.code)]\n".utf8))
            exit(1)
        }
    }

    struct Plain: LocalizedError {
        let message: String
        init(_ m: String) { message = m }
        var errorDescription: String? { message }
    }
}
