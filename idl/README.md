# idl/

The system IDL and its code generators (docs/TOOLING.md 2). One IDL source
defines the frozen kernel ABI and all typed control-plane messages, and
generates the Rust / C / Go bindings. Arrives with BUILD-ORDER.md step 6
(the queue-pair ABI is generated from it).

Each wire type also gets an Etypes-style **type hash** - identity = a
cryptographic hash of the type's description - so a protocol can be checked
at a boundary at run time, language-agnostically, not only by the generated
compile-time contract (comparison/ethos/, docs/ARCHITECTURE-DEBT.md 7.5).
