# VINO status callbacks must run outside device locks

`CdmcAdjustedSource::status()` calls `Vino::cdmc_regs()`, which locks VINO
state. Holding that state lock while requesting source status deadlocks the
monitor and prevents subsequent guest VINO register access and DMA progress.

Clone the source handles and release source locks before calling `status()`;
acquire VINO state afterward to print registers. The regression test
`status_with_cdmc_source_does_not_deadlock` uses the actual CDMC wrapper and
a bounded wait, without opening a host camera or starting a guest.

A working Test Camera preview only verifies host capture. VM source settings
are installed at machine creation, and the host source feeds D1 (IndyCam).
D0 (composite) defaults to black. Check guest capture independently of its
preview rendering before attributing a blank Media Recorder window to capture.
