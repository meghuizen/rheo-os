# tests/

In-QEMU test kernels (docs/DEVELOPMENT.md 9): each test kernel boots,
exercises one subsystem, and reports pass/fail through the QEMU exit
device. The first ones arrive with BUILD-ORDER.md step 4 (the capability
core is a verification gate).
