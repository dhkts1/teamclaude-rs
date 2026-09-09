import CoreGraphics
import Foundation

/// What the panel's authored-size arithmetic PREDICTED, beside what SwiftUI
/// actually produced — and the one line that carries both to a reader.
///
/// This is phase 3 of `docs/plans/panel-sizing-generalization.md`, and it is an
/// instrument rather than a fix. ``PanelSize`` estimates the popover's height
/// by measuring the chrome strings through TextKit, outside the view graph;
/// whether that estimate tracks what SwiftUI lays out is the one fact the whole
/// five-phase plan rests on and that nobody in the design review had. Phase 3
/// computes the estimate on every poll tick, logs it against the popover's real
/// `contentSize`, and changes no behaviour at all: `sizingOptions` is untouched
/// and the panel still sizes itself.
///
/// **Why the parts and not the total.** "The panel came out 40pt taller than we
/// said" does not name a bug. The header, the footer and the account list are
/// predicted by three different pieces of arithmetic and fail in three
/// different ways — a header wrapping to a line TextKit did not count, a footer
/// whose fixed controls were estimated at the wrong height, a row height that
/// is not the average row — so a single total leaves the reader to guess which.
/// Every part of the prediction is on the line, with the counts that generated
/// it (`rows=`, `/Nln`), because the person reading these lines is on another
/// Mac with a different fleet and gets one look at them.
///
/// **What this instrument can and cannot attribute.** From a single line it can
/// say the total error and, on a `rows=0` tick, that the whole of that error is
/// chrome — the list term is zero by construction there, so nothing else is
/// mixed in. Separating the header from the footer needs two lines whose header
/// line count differs, which a real fleet supplies on its own: every header
/// line in `FleetView` is conditional (the poll summary only on an unhealthy
/// read, the update line for two of four `UpdateState` cases, the spend line
/// only when a row carries usage), so `/Nln` moves through a session without
/// anybody arranging it. Claiming more than that from one line would be
/// arithmetic with three unknowns in it.
///
/// The maths lives here and the I/O lives in `MenuBarShell`, which is the split
/// ``PanelHeight``, ``PanelSize`` and ``UncaughtExceptionReport`` already have:
/// a log line that only exists inside an `NSLog` call is a log line no test can
/// read, and this one is the deliverable.
public struct PanelSizeDelta: Equatable, Sendable {
    /// The fixed prefix every line carries, so one
    /// `log show --predicate 'eventMessage CONTAINS "TCRBAR-PANELSIZE"'` finds
    /// all of them and nothing else. A marker rather than a subsystem because
    /// the app logs through `NSLog` throughout (`TcrBarApp.swift:72`, `:136`,
    /// `:186`) and a second logging mechanism for one phase would be a second
    /// thing to explain to the person collecting the lines.
    public static let marker = "TCRBAR-PANELSIZE"

    /// The field set, versioned. Phase 4 changes what is worth logging — the
    /// prediction stops being a shadow and starts being the assignment — and a
    /// reader holding a mixed log must be able to tell the shapes apart without
    /// counting fields.
    public static let version = "v1"

    /// What ``PanelSize/plan(rowCount:header:footer:geometry:metrics:)``
    /// returned for this tick.
    public let predicted: PanelSize.Plan

    /// The popover's own `contentSize` — `nil` when there is no measurement
    /// yet, which is not the same thing as a measurement of zero. See
    /// ``init(predicted:actualContentSize:rowCount:headerLines:footerLines:isPanelShown:)``.
    public let actual: CGSize?

    /// Accounts drawn as cards in the scrolling list.
    public let rowCount: Int

    /// How many chrome strings the header stack drew this tick. All of them are
    /// conditional, so this moves on its own — which is what makes two lines
    /// able to separate header error from footer error.
    public let headerLines: Int

    /// How many chrome strings the footer stack drew, not counting its fixed
    /// controls (the button rows and the settings checkboxes), which are
    /// reserved whether or not any footer text is drawn.
    public let footerLines: Int

    /// Whether the popover was on screen when the size was read. A
    /// `contentSize` read with the panel closed is whatever the last open left
    /// behind, so these lines are droppable — and a reader who cannot tell them
    /// apart would average a stale number into a live one.
    public let isPanelShown: Bool

    /// A non-positive `contentSize` is recorded as NO measurement.
    ///
    /// An `NSPopover` that has never been laid out reports zero. Subtracting a
    /// 533pt prediction from it yields `-533.0`, which reads exactly like a
    /// catastrophic mis-estimate and is in fact the absence of one — the single
    /// worst thing this instrument could print, because its whole job is to be
    /// believed once by somebody who cannot re-run it. Either dimension being
    /// non-positive is enough: a popover caught mid-configuration can report a
    /// width with no height, and the height is the entire subject.
    public init(
        predicted: PanelSize.Plan,
        actualContentSize: CGSize,
        rowCount: Int,
        headerLines: Int,
        footerLines: Int,
        isPanelShown: Bool
    ) {
        self.predicted = predicted
        self.actual =
            (actualContentSize.width > 0 && actualContentSize.height > 0)
            ? actualContentSize : nil
        self.rowCount = rowCount
        self.headerLines = headerLines
        self.footerLines = footerLines
        self.isPanelShown = isPanelShown
    }

    /// Positive means SwiftUI drew MORE height than predicted — the estimate
    /// was short, and under phase 4's shape the list would scroll that much
    /// early. Negative means it drew less, which under that shape leaves a void
    /// under the last row that no scroll region recovers. The sign is the
    /// finding, so it is never dropped.
    public var heightDelta: CGFloat? {
        actual.map { $0.height - predicted.height }
    }

    /// The panel has been a fixed-width column since it was written, so a
    /// non-zero width delta is a different fault from a height mis-estimate:
    /// it means the popover is not honouring the authored width at all, and
    /// every chrome string was therefore measured at a width the panel does not
    /// draw at. Logged separately because it invalidates the height figures
    /// rather than adding to them.
    public var widthDelta: CGFloat? {
        actual.map { $0.width - predicted.width }
    }

    /// The height the panel really had left after the chrome we predicted.
    ///
    /// This is the number phase 4's residual scroll region will absorb, and it
    /// is worth naming rather than leaving the reader to subtract three fields
    /// under a deadline. It differs from ``heightDelta`` by exactly the
    /// predicted list height, which is the statement the design makes in
    /// arithmetic: whatever the chrome estimate got wrong lands on the list.
    public var impliedListHeight: CGFloat? {
        actual.map { $0.height - predicted.chromeHeight }
    }

    /// One line, `key=value`, marker first.
    ///
    /// Greppable on purpose and in two directions: the marker selects the
    /// lines, and every figure is a named key so a reader can `sed`/`awk` a
    /// column out of a session's worth of them without writing a parser. It is
    /// deliberately not JSON — `log show` output is already a wrapper around
    /// each message, and a nested quote-heavy payload inside it is what makes
    /// people give up and read with their eyes.
    public var logLine: String {
        let head = [
            "\(Self.marker) \(Self.version)",
            "shown=\(isPanelShown ? "yes" : "no")",
            "rows=\(rowCount)",
            "header=\(Self.pt(predicted.headerHeight))/\(headerLines)ln",
            "footer=\(Self.pt(predicted.footerHeight))/\(footerLines)ln",
            "list=\(Self.pt(predicted.listHeight))",
            "frame=\(Self.pt(predicted.frameHeight))",
            "predicted=\(Self.pt(predicted.width))x\(Self.pt(predicted.height))",
        ]
        let tail = [
            "actual=\(actual.map { "\(Self.pt($0.width))x\(Self.pt($0.height))" } ?? "unlaid")",
            "delta-height=\(heightDelta.map(Self.signed) ?? "unknown")",
            "delta-width=\(widthDelta.map(Self.signed) ?? "unknown")",
            "implied-list=\(impliedListHeight.map(Self.pt) ?? "unknown")",
        ]
        return (head + tail).joined(separator: " ")
    }

    /// One decimal place, always. The panel's own grain is 0.5pt
    /// (``PanelHeight/measurementGrain``), so a whole-point figure would round
    /// away the half-points two `Hairline`s put into every total, and more
    /// places would print float noise as if it were a measurement.
    private static func pt(_ value: CGFloat) -> String {
        String(format: "%.1f", Double(value))
    }

    /// A delta always carries its sign, `+0.0` included: an unsigned zero and a
    /// missing figure look the same in a column of numbers, and "exactly right"
    /// is a result worth being able to see.
    private static func signed(_ value: CGFloat) -> String {
        String(format: "%+.1f", Double(value))
    }
}
