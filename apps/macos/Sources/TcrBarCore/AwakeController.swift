import Combine
import Foundation
import IOKit.pwr_mgt

/// "Keep this Mac awake" — what `caffeinate -i -m -s` does, held for exactly as
/// long as the control is on.
///
/// ## Mechanism, measured rather than assumed
///
/// Three power assertions taken together through
/// `IOPMAssertionCreateWithName`, each named with ``reason``:
/// `PreventUserIdleSystemSleep` (`-i`), `PreventSystemSleep` (`-s`) and
/// `PreventDiskIdle` (`-m`). That is the set `caffeinate -i -m -s` registers
/// under its own pid, which is checkable in one line:
///
///     caffeinate -i -m -s -t 8 & sleep 2; pmset -g assertions | grep -A1 caffeinate
///
/// `ProcessInfo.beginActivity(options: .idleSystemSleepDisabled,)` was the
/// previous mechanism and is why this note exists. It can express `-i` and
/// nothing else — the API has no option corresponding to `-s` or to `-m` — so
/// the checkbox could not match the command line an operator actually trusts.
///
/// `PreventSystemSleep` is spelled as a raw string on purpose.
/// `kIOPMAssertionTypePreventSystemSleep` is marked deprecated in `IOPMLib.h`
/// and there is no modern replacement constant, yet `caffeinate` registers the
/// same string today and `powerd` honours it. Deleting it because the header
/// disapproves would silently drop `-s`.
///
/// ``KeepAwakeProbe`` is the flag that produces those lines from a shell, and
/// it is the gate: the claim is checkable in about fifteen seconds and should
/// be re-checked rather than believed.
///
/// ## Deliberate scope
///
/// **Idle *system* sleep only, never display sleep.** The job is "a long run
/// keeps running", and the screen going dark does not stop a run. Holding the
/// display awake all night is a real cost — backlight, burn-in, a bright room —
/// that nobody asked for. `PreventUserIdleDisplaySleep` exists and is not used.
///
/// **`PreventSystemSleep` is AC-only.** Per `man caffeinate`, `-s` "is valid
/// only when system is running on AC power", and the power log agrees: the same
/// assertion held on AC shows `[System: PrevIdle PrevSleep kCPU]` while on
/// battery it shows no `PrevSleep` at all. On battery this control is `-i -m`.
///
/// Whether the assertion survives closing the lid on AC is **not known here**.
/// No measurement of that exists on this machine, so neither the doc-comment
/// nor the panel claims an answer in either direction.
///
/// **Persisted across launches**, and it was not. The reversal is Gil's call
/// (2026-08-30) and the reasoning it overturns is worth keeping, because it was
/// not wrong about the danger:
///
/// > A Mac that silently never sleeps because of a box ticked a week ago is a
/// > worse bug than having to tick it again — the symptom (a laptop that runs
/// > hot in a bag) is nowhere near the cause.
///
/// What makes it safe now is that the state is no longer silent. ``MenuBarMark``
/// draws a tinted cup beside the gauge for exactly as long as the assertions are
/// held, in the menu bar, on every screen the operator looks at — the shape
/// channel survives greyscale and colour vision deficiency, and the status
/// item's tooltip names it too. The cause is not "nowhere near the symptom"; it
/// is the one glyph that is always on screen. A setting whose whole job is "this
/// machine stays up for long runs" and that has to be re-armed after every
/// reboot is a setting that is off exactly when it was needed.
///
/// What is stored is the operator's INTENT — what ``setOn(_:)`` was asked for —
/// not ``isOn``, which mirrors the token and is a fact about the machine.
/// A take that fails leaves the panel honestly OFF and the stored intent ON, so
/// the next launch tries again rather than quietly disabling itself. That split
/// is also what keeps ``releaseOnQuit()`` from wiping the preference on the way
/// out: quitting ends the assertions, it does not un-ask for them.
///
/// ``defaults`` is optional and `nil` means REMEMBER NOTHING — the PNG harness
/// and the shell probe both drive `setOn` to draw the ON state, and a render must
/// not write the operator's preferences. See ``AwakeController/harness()``.
@MainActor
public final class AwakeController: ObservableObject {

    /// The three assertion types, in the order they are taken.
    ///
    /// Exposed because the gate needs to name them: `pmset -g assertions` is
    /// the only thing that can see a power assertion, and a gate that greps for
    /// one of three passes on a controller that dropped the other two.
    public static let assertionTypes: [String] = [
        kIOPMAssertPreventUserIdleSystemSleep as String,
        // No modern constant exists; see the note above before "fixing" this.
        "PreventSystemSleep",
        kIOPMAssertPreventDiskIdle as String,
    ]

    /// Every assertion id of one hold, released together.
    ///
    /// One object for the whole set is what keeps partial state
    /// unrepresentable: the controller stores one token, so it cannot end up
    /// holding two of three assertions while reporting ON, and cannot release
    /// some of them and forget the rest.
    public final class Held: NSObject {
        fileprivate let ids: [IOPMAssertionID]

        fileprivate init(ids: [IOPMAssertionID]) {
            self.ids = ids
        }
    }

    /// The begin/end pair, injected.
    ///
    /// Nothing in-process can observe another process's power assertions, so a
    /// test against the real IOKit could only prove that the calls did not
    /// throw — while leaving the machine awake for as long as the suite ran.
    /// Taking the pair as a dependency is what makes the property that actually
    /// matters testable: that begin and end are called exactly once each, on the
    /// same token. `LoginItem.classify` is the precedent — keep the part with a
    /// decision in it separable from the part that talks to the system.
    public struct Activity {
        /// `nil` means nothing is held.
        ///
        /// Unlike `beginActivity`, `IOPMAssertionCreateWithName` returns an
        /// `IOReturn` and can fail. All-or-nothing is the only honest answer:
        /// reporting ON while holding two of three assertions would be the
        /// panel lying about the state of the machine, which is the defect this
        /// whole class is shaped around.
        public let begin: (String) -> NSObjectProtocol?
        public let end: (NSObjectProtocol) -> Void

        public init(
            begin: @escaping (String) -> NSObjectProtocol?,
            end: @escaping (NSObjectProtocol) -> Void
        ) {
            self.begin = begin
            self.end = end
        }

        /// The real one.
        public static let powerAssertions = Activity(
            begin: { reason in
                var ids: [IOPMAssertionID] = []
                for type in AwakeController.assertionTypes {
                    var id: IOPMAssertionID = IOPMAssertionID(kIOPMNullAssertionID)
                    let result = IOPMAssertionCreateWithName(
                        type as CFString,
                        IOPMAssertionLevel(kIOPMAssertionLevelOn),
                        reason as CFString,
                        &id)
                    guard result == kIOReturnSuccess else {
                        // Unwind, so a failure half way through the set leaves
                        // the machine exactly as it was found.
                        for taken in ids { IOPMAssertionRelease(taken) }
                        return nil
                    }
                    ids.append(id)
                }
                return Held(ids: ids)
            },
            end: { token in
                guard let held = token as? Held else { return }
                for id in held.ids { IOPMAssertionRelease(id) }
            }
        )

        /// A pair that holds nothing.
        ///
        /// For the PNG harness, which has to draw the panel in its ON state and
        /// must not stop the machine sleeping in order to do it. Rendering a
        /// checkbox is not a reason to hold a power assertion.
        public static let inert = Activity(begin: { _ in NSObject() }, end: { _ in })
    }

    /// The assertions' name in `pmset -g assertions`, and therefore the string a
    /// human greps for when their Mac will not sleep and they want to know who
    /// is holding it open.
    ///
    /// Stable and greppable on purpose: it contains "TcrBar", the gate in
    /// `apps/macos/README.md` matches on that, and rewording it silently breaks
    /// both the gate and the human's search.
    public static let reason = "TcrBar is keeping this Mac awake"

    /// Whether the assertions are held. Read by the panel and the menu bar.
    @Published public private(set) var isOn = false

    /// The live token — the single source of truth, and the only thing that can
    /// move ``isOn``.
    ///
    /// A separate `Bool` is the defect this shape exists to prevent: any path
    /// that set the flag without moving the token (or the reverse) would leave
    /// the panel reporting a state of the *machine* that is not true. Because
    /// the mirror is maintained here and nowhere else, "the checkbox is ticked"
    /// and "the assertions are held" cannot disagree — including when the take
    /// fails, which stores `nil` and therefore reads OFF.
    private var token: NSObjectProtocol? {
        didSet { isOn = token != nil }
    }

    private let activity: Activity

    /// The `UserDefaults` key. Do not change it — renaming it fails nothing and
    /// silently un-arms a setting the operator chose, the same trap
    /// ``LaunchPreference/startServerAtLaunchKey`` documents. `AwakeControllerTests`
    /// pins the literal.
    public static let keepAwakeKey = "keepThisMacAwake"

    /// `nil` remembers nothing. See the type's doc-comment.
    private let defaults: UserDefaults?

    public init(activity: Activity = .powerAssertions, defaults: UserDefaults? = .standard) {
        self.activity = activity
        self.defaults = defaults
    }

    /// The pairing every harness wants: hold nothing, remember nothing.
    ///
    /// Named rather than left to two call sites to assemble, because the second
    /// half is the one that is easy to forget and the damage is invisible —
    /// rendering the ON scene with real defaults would write
    /// ``keepAwakeKey`` into the operator's own preferences.
    public static func harness() -> AwakeController {
        AwakeController(activity: .inert, defaults: nil)
    }

    /// Take or release the assertions, and remember which was asked for.
    ///
    /// The store is written BEFORE the attempt on purpose: it records what was
    /// asked, and a take that fails is still a request to keep this Mac awake.
    public func setOn(_ on: Bool) {
        defaults?.set(on, forKey: Self.keepAwakeKey)
        if on { begin() } else { end() }
    }

    /// Re-arm at launch from the stored intent. Called once, from
    /// `applicationDidFinishLaunching`, beside the `startServerAtLaunch` spawn.
    ///
    /// Deliberately NOT done in `init`: the render harness and the shell probe
    /// build a controller too, and a constructor that reaches for the operator's
    /// preferences would have them silently holding — or drawing — a state
    /// nobody asked those processes for. It also goes through ``begin()``
    /// rather than ``setOn(_:)``, so restoring writes nothing back.
    public func restoreFromPreference() {
        guard defaults?.bool(forKey: Self.keepAwakeKey) == true else { return }
        begin()
    }

    public func toggle() {
        setOn(!isOn)
    }

    /// Idempotent, and that is the whole point.
    ///
    /// A second take would return a second set of assertion ids, and this class
    /// can only store one — so the first set would leak, unreleasable for the
    /// lifetime of the process. The panel would then show OFF while the Mac
    /// still refused to sleep: a control that lies about the state of the
    /// machine, with no way back short of quitting the app.
    private func begin() {
        guard token == nil else { return }
        token = activity.begin(Self.reason)
    }

    /// Ends only a token this class began, and clears it as part of ending it.
    ///
    /// The clear happens *first* so there is no instant in which `token` names
    /// assertions that have already been released — which is the state from
    /// which a double release of the same id becomes possible.
    private func end() {
        guard let held = token else { return }
        token = nil
        activity.end(held)
    }

    /// Called from `applicationWillTerminate`.
    ///
    /// The kernel releases every power assertion a process holds when it dies,
    /// so this is not what lets the Mac sleep again — that would happen anyway.
    /// It is here because "quitting TcrBar releases the assertions" should be
    /// something this app *does*, not something it gets away with.
    public func releaseOnQuit() {
        end()
    }
}
