# Hoofdstuk 11 - Interrupts van dichtbij

In hoofdstuk 10 noemden we **interrupts** als volgende stap op de landkaart. Nu
duiken we er echt in. Interrupts zijn het antwoord op een simpele vraag: hoe weet
de processor dat er iets gebeurd is in de buitenwereld - een toets die wordt
ingedrukt, een klok die tikt, een netwerkpakketje dat aankomt - zonder constant
zelf te gaan kijken?

## Pollen: de vermoeiende manier

Stel je voor dat je op een pakketje wacht. Je kunt elke minuut naar de voordeur
lopen om te kijken of het er al is. Dat heet **pollen** (steeds zelf checken). Het
werkt, maar je kunt ondertussen niks anders doen. Onze eerste bootloader deed dit
eigenlijk al: in een lus steeds de seriële poort lezen om te kijken of er een
letter was.

```text
+----------+          +------------+
| Processor| -pollen->| Toetsenbord|
| "Is er   |          | "Nee."     |
|  een     |          +------------+
|  toets?" |
|          | -pollen->| "Nee."     |
|          |          +------------+
|          | -pollen->| "Ja! 'A'"  |
+----------+          +------------+
```

## Interrupts: de deurbel

Nu de betere manier. Je hangt een deurbel op. Je gaat rustig verder met je werk.
De bel gaat als er iemand is. Dat is precies wat een **interrupt** doet: de
hardware trekt aan een draadje (letterlijk een elektrisch signaal), en de
processor stopt met wat hij bezig was om het af te handelen.

```text
Processor werkt rustig aan programma A
           |
     ===== | =================== interrupt-signaal! ===
           |
           v
Processor slaat zijn plek op (registers, program counter)
           |
           v
Processor springt naar de interrupt-handler
           |
           v
Handler doet het werk (lees de toets, verwerk het)
           |
           v
Processor herstelt zijn oude plek en gaat verder met A
```

Het mooie: programma A merkt er helemaal niks van. Het is alsof je een boek
leest, even de deurbel opendoet, en precies op hetzelfde woord verder gaat.

## Wat er in de processor gebeurt

Als een interrupt binnenkomt, doet de processor een paar dingen heel snel,
automatisch, in de hardware:

1. **Stop de huidige instructie.** Zodra de huidige instructie klaar is (hij
   maakt hem nog af), stopt de processor met de volgende.
2. **Sla de huidige plek op.** De processor bewaart de **program counter**
   (waar was ik?) en soms ook een stukje van de status (was ik in kernel-stand
   of gebruikers-stand? Welke interrupts waren aan?). Bij RISC-V gaat dit in
   speciale registers (`sepc`, `sstatus`). Bij ARM64 in `ELR_EL1` en
   `SPSR_EL1`. Bij x86 op de stack.
3. **Spring naar de handler.** De processor kijkt in een tabel: "voor dit type
   interrupt, spring naar deze code." Die tabel heet de **vectortabel** of
   **interrupt descriptor table** (IDT op x86).
4. **De handler draait.** Jouw code handelt het af: lees de toets, verwerk de
   klok, wat er nodig is.
5. **Terugkeren.** De handler herstelt alles en geeft de processor terug met
   een speciale instructie (`sret` op RISC-V, `eret` op ARM64, `iret` op x86).
   De processor pakt de opgeslagen program counter op en gaat verder alsof er
   niks gebeurd is.

## De vectortabel: welke handler bij welke interrupt?

De processor moet weten *waar* hij naartoe springt. Dat staat in een tabel met
adressen, een voor elk soort interrupt. Dit heet een **vectortabel**.

```text
Vectortabel (vereenvoudigd)
+--------+-----------------------------+
| nr  0  | adres van handler_nul       |
| nr  1  | adres van handler_een       |
| nr  2  | adres van handler_twee      |
| ...    | ...                         |
| nr 33  | adres van uart_handler      |  <- "er is een teken op de seriele poort"
| nr 48  | adres van timer_handler     |  <- "de timer tikte"
+--------+-----------------------------+
```

De processor leest de tabel op het moment dat de interrupt binnenkomt, kijkt
welk nummer het is, en springt naar het bijbehorende adres. In rheo-os vind je
de vectortabellen in `kernel/arch/` - per ISA een apart bestand.

## Interrupt-controllers: de verkeersregelaars

Hardware-apparaten (toetsenbord, timer, netwerkkaart) zijn niet rechtstreeks met
de processor verbonden. Er zit een **interrupt-controller** tussen. Dat is een
apart stukje hardware dat alle signalen opvangt, een nummer (vector) toekent,
en het aan de processor doorgeeft.

Elke processorarchitectuur heeft zijn eigen interrupt-controller:

- **x86**: vroeger de **PIC** (Programmable Interrupt Controller), tegenwoordig
  de **APIC** (Advanced PIC). De APIC zit *in* elke processor en heeft een
  apart stuk (**IO-APIC**) voor externe apparaten.
- **ARM64**: de **GIC** (Generic Interrupt Controller). Die heeft een
  **distributor** (GICD) die interrupts over processoren verdeelt, en een
  **redistributor** (GICR) per processor.
- **RISC-V**: de **PLIC** (Platform-Level Interrupt Controller) voor externe
  apparaten, en op nieuwere chips de **APLIC** + **IMSIC** die op de RISC-V AIA
  standaard zijn gebaseerd.

Ondanks de verschillende namen doen ze allemaal hetzelfde:

```text
+-----------+     +---------------------+     +-----------+
| Toetsen-  |---->|                     |     |           |
| bord      |     | Interrupt-controller|---->| Processor |
+-----------+     | (PIC/APIC/GIC/PLIC) |     |           |
+-----------+     |                     |     +-----------+
| Timer     |---->| Kiest de            |
+-----------+     | belangrijkste       |
+-----------+     | en geeft een nummer  |
| Netwerk-  |---->|                     |
| kaart     |     +---------------------+
+-----------+
```

## Prioriteiten

Wat als twee interrupts tegelijk komen? De controller kiest welke het eerst aan
de beurt is op basis van **prioriteit**. Een timer-interrupt heeft vaak een hoge
prioriteit (die mag niet wachten), terwijl een toets iets lager zit (een paar
milliseconden later is geen probleem).

De processor kan ook interrupts tijdelijk **uitzetten** (maskeren). Dat is
belangrijk: als de handler voor interrupt A bezig is, wil je niet dat interrupt B
er halverwege doorheen springt. In rheo-os wordt dit per ISA geregeld: RISC-V
zet een bit in `sstatus`, ARM64 gebruikt `daif`, x86 heeft de `IF`-vlag.

## Pollen vs. interrupts: een eerlijke vergelijking

| Eigenschap        | Pollen                      | Interrupts                  |
|-------------------|-----------------------------|-----------------------------|
| Werkwijze         | Steeds zelf kijken          | Hardware geeft een signaal  |
| CPU-gebruik       | Hoog (constante lus)        | Laag (wacht rustig)         |
| Snelheid          | Hangt af van hoe vaak je kijkt | Bijna direct            |
| Moeilijkheid      | Simpel                      | Lastiger in te richten      |
| Geschikt voor     | Snel testen, simpele code   | Echt OS, meerdere bronnen   |

In de praktijk gebruiken besturingssystemen interrupts voor bijna alles, en
pollen alleen in speciale gevallen (bijvoorbeeld heel snelle netwerken waar elke
microseconde telt).

## De timer-interrupt: de sleutel tot multitasking

Van alle interrupts is er een die extra belangrijk is: de **timer-interrupt**.
Dit is een klokje in de processor dat op regelmatige tijden een interrupt
genereert - bijvoorbeeld elke milliseconde.

Waarom is dat zo belangrijk? Denk terug aan hoofdstuk 1: het OS moet
programma's laten afwisselen. Maar wat als een programma niet vrijwillig stopt?
De timer-interrupt dwingt het af: elke paar milliseconden wordt het
programma onderbroken, en de kernel krijgt de kans om een ander programma aan de
beurt te laten. Zonder de timer zou een programma dat in een lus hangt nooit
meer de processor loslaten.

```text
timer   timer   timer   timer
  |       |       |       |
  v       v       v       v
AAAA|BBBBB|AAAA|CCCC|BBBBB|...
          ^              ^
          |              |
     A onderbroken   B onderbroken
     B krijgt beurt  C krijgt beurt
```

In rheo-os is de timer op elke ISA anders:

- **RISC-V**: het `stimecmp`-register (Sstc-extensie) genereert een interrupt
  als de klok die waarde bereikt.
- **ARM64**: de virtuele timer (`CNTV`) met een PPI door de GICv3.
- **x86**: de LAPIC one-shot timer, gestuurd via MMIO-registers.

De kernel-code voor de timer en het inplannen van programma's vind je in
`kernel/src/ktimer.rs` (de timer-arbiter) en `kernel/src/sched/` (de scheduler).

## Interrupts in rheo-os

In rheo-os werkt het zo:

- Elke ISA heeft zijn eigen vectortabel in `kernel/arch/` (assembly).
- De handler slaat de **trap frame** op (alle registers van het onderbroken
  programma) en roept een Rust-functie aan. Die struct heet `TrapFrame` en
  staat in `kernel/src/arch/<isa>/mod.rs`.
- De ontvangen bytes van het toetsenbord belanden in een **RX-ring** in
  `kernel/src/input.rs` - een ringbuffer waar de handler schrijft en de
  wachtende code leest.
- Na afhandeling herstelt de handler de registers en keert terug met de
  ISA-specifieke terugkeer-instructie.

## Samenvatting

- **Pollen** is steeds zelf kijken of er iets is; **interrupts** zijn de
  deurbel: de hardware meldt het zelf.
- Bij een interrupt slaat de processor automatisch zijn plek op, springt naar
  een **handler** uit de **vectortabel**, en keert daarna terug.
- Een **interrupt-controller** (APIC, GIC, PLIC) zit tussen de apparaten en de
  processor, kent nummers toe en regelt **prioriteiten**.
- De **timer-interrupt** is de basis voor multitasking: hij dwingt programma's
  om de processor los te laten.
- In rheo-os staan de vectortabellen in `kernel/arch/`, de trap-afhandeling in
  `kernel/src/arch/<isa>/mod.rs`, en de byte-ontvangst in `kernel/src/input.rs`.

## Oefeningen

1. Leg in je eigen woorden uit waarom pollen inefficient is als je op meerdere
   apparaten tegelijk wilt wachten (toetsenbord, netwerk en timer).
2. Wat zou er gebeuren als de processor bij een interrupt de program counter
   *niet* bewaarde? Waarom is dat stap zo belangrijk?
3. Waarom is de timer-interrupt onmisbaar voor een echt besturingssysteem met
   meerdere programma's?
4. Zoek in `kernel/arch/riscv64/` het bestand met de vectortabel. Hoeveel
   verschillende soorten traps onderscheidt RISC-V?
5. Een interrupt komt binnen terwijl de kernel al een andere interrupt afhandelt.
   Wat zou er fout gaan als de kernel interrupts niet tijdelijk uitzet?

Door naar [hoofdstuk 12](12-virtueel-geheugen.md): hoe elk programma denkt dat
het al het geheugen voor zichzelf heeft.
