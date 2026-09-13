import XCTest

@testable import TcrBarCore

/// The machine line's two testable halves: the sentence it draws, and the
/// band it tints. ``MachineStats/read()`` itself is not tested — it reads this
/// machine, and a test that asserts anything about this machine's load average
/// asserts about the CI runner instead.
final class MachineStatsTests: XCTestCase {
    private func stats(
        load: Double = 1.0,
        cores: Int = 14,
        used: UInt64 = 48 * 1_073_741_824,
        total: UInt64 = 64 * 1_073_741_824,
        compiles: Int = 5,
        free: UInt64 = 210_000_000_000
    ) -> MachineStats {
        MachineStats(
            loadAverage: load, cores: cores, memoryUsedBytes: used, memoryTotalBytes: total,
            compiles: compiles, diskFreeBytes: free)
    }

    /// `docs/design/tools-tab.md`'s machine line, verbatim.
    func testLineMatchesTheMockup() {
        XCTAssertEqual(
            stats(load: 7.1).line,
            "load 7.1/14 · 48/64 GB · 5 compiles · 210 GB free")
    }

    /// The clause a view tints and the rest of the line join back into the
    /// whole line — the two halves exist so the tint can stop at the load,
    /// not so the sentence can differ from itself.
    func testClausesComposeTheLine() {
        let machine = stats(load: 7.1)
        XCTAssertEqual("\(machine.loadClause) · \(machine.restClause)", machine.line)
    }

    func testOneCompileIsSingular() {
        XCTAssertTrue(stats(compiles: 1).line.contains("· 1 compile ·"))
        XCTAssertTrue(stats(compiles: 0).line.contains("· 0 compiles ·"))
    }

    /// Memory reads in binary GB (a Mac's own "64 GB of RAM"), disk in decimal
    /// GB (the disk's own capacity, and Finder's). Both rounded to whole
    /// units: a tenth of a gigabyte changes no decision.
    func testUnitsAreTheOnesEachFigureIsQuotedIn() {
        let machine = stats(
            used: 17_179_869_184,  // 16 GiB
            total: 68_719_476_736,  // 64 GiB
            free: 210_400_000_000)  // 210 GB
        XCTAssertTrue(machine.line.contains("16/64 GB"), machine.line)
        XCTAssertTrue(machine.line.contains("210 GB free"), machine.line)
    }

    /// Gil's own dispatch rule: below one load unit per core says nothing,
    /// up to twice cores is amber, past twice cores is red and means one lane
    /// rather than three.
    func testLoadTintBands() {
        XCTAssertEqual(stats(load: 13.9, cores: 14).loadTint, .calm)
        XCTAssertEqual(stats(load: 14.0, cores: 14).loadTint, .busy)
        XCTAssertEqual(stats(load: 28.0, cores: 14).loadTint, .busy)
        XCTAssertEqual(stats(load: 28.1, cores: 14).loadTint, .overloaded)
    }

    /// A core count of zero is a failed read, not an infinitely overloaded
    /// box: dividing by it must not colour the line red.
    func testZeroCoresIsCalmNotRed() {
        XCTAssertEqual(stats(load: 7.1, cores: 0).loadTint, .calm)
    }
}
