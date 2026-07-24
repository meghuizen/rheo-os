# idl/

The system IDL and its code generators (docs/TOOLING.md 2). One IDL source
defines the frozen kernel ABI and all typed control-plane messages, and
generates the Rust / C / Go bindings. Arrives with BUILD-ORDER.md step 6
(the queue-pair ABI is generated from it).
