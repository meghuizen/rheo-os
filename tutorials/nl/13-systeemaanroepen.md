# Hoofdstuk 13 - Systeemaanroepen van binnen

In hoofdstuk 1 leerden we dat een programma in gebruikers-stand niet zomaar bij
de hardware mag. Het moet het netjes vragen aan de kernel. Dat vragen heet een
**systeemaanroep** (Engels: *system call*, vaak afgekort tot *syscall*). In dit
hoofdstuk kijken we precies hoe zo'n aanroep werkt, van de eerste instructie in
het programma tot het antwoord van de kernel.

## Het probleem: hoe roep je code aan die je niet mag uitvoeren?

Een programma draait in gebruikers-stand. De kernel draait in kernel-stand. Die
twee standen zijn bewust gescheiden: de processor laat een gebruikersprogramma
niet zomaar naar kernel-code springen.

Maar het programma *moet* de kernel kunnen bereiken. Het wil een bestand openen,
tekst op het scherm zetten, of een random getal vragen.

De oplossing: een speciale instructie die zegt "ik wil de kernel spreken." De
processor schakelt dan over naar kernel-stand en springt naar een vast
beginpunt. Dat overschakelen heet een **trap** (val). Het lijkt op een interrupt
(hoofdstuk 11), maar het verschil is: een interrupt komt van *buiten* (hardware),
en een trap wordt door het programma *zelf* veroorzaakt.

## De trap-instructie per ISA

Elke processorarchitectuur heeft zijn eigen instructie voor een syscall:

| ISA      | Instructie | Wat de processor doet                       |
|----------|------------|---------------------------------------------|
| x86-64   | `syscall`  | Springt naar het adres in het `LSTAR`-register |
| ARM64    | `svc #0`   | Genereert een Supervisor Call exception      |
| RISC-V   | `ecall`    | Genereert een Environment Call exception     |

De namen zijn verschillend, maar het effect is hetzelfde: de processor schakelt
naar kernel-stand en springt naar een vaste plek waar de kernel code klaarstaat.

## De ABI: afspraken over registers

Het programma moet de kernel twee dingen vertellen:

1. **Wat wil je?** - het **syscall-nummer**. Elk soort verzoek heeft een nummer.
   Bijvoorbeeld: `write` (schrijven naar een bestand) is op Linux-x86-64 nummer
   1. Op ARM64 en RISC-V is het nummer 64.
2. **Waarmee?** - de **argumenten**. Bijvoorbeeld: naar welk bestand, welke data,
   hoeveel bytes.

Waar stopt het programma die informatie? In **registers**. Dat is veel sneller
dan het geheugen, en de processor kan ze meteen lezen. De precieze afspraak over
welk register wat draagt heet de **ABI** (Application Binary Interface).

```text
Registers bij een syscall (vereenvoudigd):

x86-64:                  ARM64:                   RISC-V:
+-------+-----------+    +-------+-----------+    +-------+-----------+
| rax   | nummer    |    | x8    | nummer    |    | a7    | nummer    |
| rdi   | arg 1     |    | x0    | arg 1     |    | a0    | arg 1     |
| rsi   | arg 2     |    | x1    | arg 2     |    | a1    | arg 2     |
| rdx   | arg 3     |    | x2    | arg 3     |    | a2    | arg 3     |
| r10   | arg 4     |    | x3    | arg 4     |    | a3    | arg 4     |
| r8    | arg 5     |    | x4    | arg 5     |    | a4    | arg 5     |
| r9    | arg 6     |    | x5    | arg 6     |    | a5    | arg 6     |
+-------+-----------+    +-------+-----------+    +-------+-----------+

Resultaat terug:           Resultaat terug:         Resultaat terug:
  rax                        x0                       a0
```

Na de syscall zet de kernel het antwoord (een getal: gelukt of een foutcode)
in een afgesproken register, en het programma leest het daar uit.

## De reis van een write()-aanroep

Laten we stap voor stap volgen wat er gebeurt als een C-programma
`write(1, "hallo", 5)` aanroept. Dat betekent: schrijf 5 bytes van de tekst
"hallo" naar bestand 1 (dat is de standaard-uitvoer, je scherm).

```text
1. Het C-programma roept write() aan
         |
         v
2. De C-bibliotheek (libc) zet de argumenten in registers:
   - syscall-nummer (bv. 64 op RISC-V) in a7
   - bestandsnummer (1) in a0
   - adres van "hallo" in a1
   - lengte (5) in a2
         |
         v
3. De C-bibliotheek voert de trap-instructie uit: ecall
         |
         v
============== GRENS: gebruiker -> kernel ==============
         |
         v
4. De processor:
   - schakelt naar kernel-stand (S-mode op RISC-V)
   - slaat de program counter op (in sepc)
   - springt naar de kernel's trap-handler
         |
         v
5. De kernel's trap-handler:
   - slaat ALLE registers van het programma op (de "trap frame")
   - leest het syscall-nummer uit a7
   - roept de juiste kernelfunctie aan
         |
         v
6. De kernelfunctie:
   - controleert: mag dit programma naar bestand 1 schrijven?
   - leest 5 bytes van het opgegeven adres
   - stuurt ze naar de seriele poort (of het scherm)
   - zet het resultaat (5 = "5 bytes geschreven") in a0 van de trap frame
         |
         v
7. De trap-handler:
   - herstelt alle registers van het programma
   - voert sret uit (terugkeer naar gebruikers-stand)
         |
         v
============== GRENS: kernel -> gebruiker ==============
         |
         v
8. Het programma gaat verder
   - a0 bevat 5 (het antwoord: 5 bytes geschreven)
   - het programma merkt niet dat het even in de kernel was
```

## De trap frame: alles bewaren

Bij stap 5 moet de kernel *alle* registers van het programma opslaan. Waarom?
Omdat de kernel zelf ook registers nodig heeft om te werken. Als de kernel
register `a1` gebruikt voor zijn eigen berekening, is de waarde van het
programma weg. Straks, bij het terugkeren, moet alles weer precies hetzelfde
zijn.

Die opgeslagen registers heten samen een **trap frame** (of *exception frame*).
Het is een blok geheugen met daarin een kopie van elk register. In rheo-os heet
dit de `TrapFrame`-struct, en elke ISA heeft zijn eigen versie in
`kernel/src/arch/<isa>/mod.rs`.

```text
TrapFrame (vereenvoudigd):
+----+--------+
| pc | 0x4020 |  <- waar het programma was
| sp | 0x7FF0 |  <- de stack pointer
| a0 | 1      |  <- argument 1 (bestandsnummer)
| a1 | 0x2000 |  <- argument 2 (adres van "hallo")
| a2 | 5      |  <- argument 3 (lengte)
| a7 | 64     |  <- syscall-nummer
| ...| ...    |  <- alle andere registers
+----+--------+
```

## De functie decode_syscall

Nadat de registers zijn opgeslagen, moet de kernel uitzoeken welke syscall het
is. In rheo-os doet de functie `decode_syscall` dit: hij leest het
syscall-nummer uit de trap frame en geeft het terug samen met de zes argumenten.

De code in `kernel/src/arch/<isa>/mod.rs` weet voor elke ISA welk register het
nummer draagt (x86: `rax`, ARM64: `x8`, RISC-V: `a7`) en welke registers de
argumenten dragen. Zo hoeft de rest van de kernel niet te weten op welke
processor hij draait.

## Beveiliging: de kernel vertrouwt het programma niet

Een belangrijk punt: de kernel mag *nooit* blindelings doen wat het programma
vraagt. Het programma kan foute of kwaadaardige waarden meegeven:

- Een adres dat buiten het geheugen van het programma valt.
- Een bestandsnummer dat niet bestaat.
- Een lengte die veel te groot is.

Daarom controleert de kernel alles. In rheo-os zit die controle in
`kernel/src/user.rs` - functies als `user_read_ok` en `user_write_ok` die
checken of een adres echt van het programma is, voordat de kernel het aanraakt.

## Systeemaanroepen in rheo-os

In rheo-os zijn syscalls het fundament van alles wat een programma doet:

- De nummers en het formaat staan in `abi/src/lib.rs` - een apart pakket zodat
  kernel en programma dezelfde nummers delen.
- De trap komt binnen in de per-ISA handler (`kernel/arch/<isa>/vectors.S` of
  `user.S`), die de trap frame opslaat en `on_user_trap` aanroept.
- `on_user_trap` in `kernel/src/user.rs` is het centrale punt: het leest het
  syscall-nummer, roept `decode_syscall` aan, en stuurt het door naar de juiste
  afhandelaar.
- Het resultaat gaat terug via de trap frame, en de terugkeer-instructie
  (`sret`/`eret`/`sysretq`) brengt het programma terug naar gebruikers-stand.

## Samenvatting

- Een **systeemaanroep** is hoe een programma de kernel om hulp vraagt.
- Het programma zet het syscall-nummer en de argumenten in **registers**
  volgens een vaste **ABI**, en voert dan een **trap-instructie** uit
  (`syscall`/`svc`/`ecall`).
- De processor schakelt naar kernel-stand en springt naar de handler.
- De kernel slaat alle registers op in een **trap frame**, verwerkt het
  verzoek, zet het resultaat in een register, en keert terug.
- De kernel **controleert alles**: adressen, nummers, lengtes. Een programma
  wordt nooit vertrouwd.
- In rheo-os staat het centrale punt in `kernel/src/user.rs` (`on_user_trap`),
  de ABI in `abi/src/lib.rs`, en de per-ISA trap-code in `kernel/arch/`.

## Oefeningen

1. Waarom kan een programma niet gewoon direct naar een kernelfunctie springen,
   zoals het een eigen functie aanroept?
2. Op RISC-V draagt register `a7` het syscall-nummer. Op x86-64 is dat `rax`.
   Waarom maakt dat voor de rest van de kernel niet uit?
   (Hint: denk aan `decode_syscall`.)
3. Teken de reis van een `read()`-aanroep (lees 10 bytes van het toetsenbord)
   in dezelfde stijl als het `write()`-voorbeeld hierboven. Welke registers
   worden gevuld, en wat komt er terug?
4. Wat zou er gebeuren als de kernel de trap frame *niet* herstelde bij het
   terugkeren? Welk effect heeft dat op het programma?
5. Waarom moet de kernel controleren of het adres dat het programma meegeeft
   echt binnen het geheugen van dat programma valt?

Door naar [hoofdstuk 14](14-context-wisselen.md): hoe de processor van het ene
programma naar het andere overschakelt.
