# Hoofdstuk 14 - Context wisselen: de processor van eigenaar laten wisselen

In hoofdstuk 1 leerden we dat het OS programma's razendsnel laat afwisselen. In
hoofdstuk 11 zagen we dat de timer-interrupt de processor regelmatig onderbreekt.
Maar *hoe* schakelt de processor van programma A naar programma B? Dat heet
een **context switch** (contextwisseling), en het is een van de belangrijkste
trucs in een besturingssysteem.

## Wat is een "context"?

De **context** van een programma is alles wat dat programma uniek maakt op het
moment dat het draait. Als je de context van programma A zou opschrijven op een
briefje, en later dat briefje weer zou inlezen, dan kan programma A precies
verder waar het was. De context bevat:

- Alle **registers** (de zakjes uit hoofdstuk 2): de getallen waar het
  programma mee aan het rekenen was.
- De **program counter** (PC): welke instructie aan de beurt was.
- De **stack pointer** (SP): waar de stapel van het programma zit.
- De **paginatabel-pointer**: welke adresvertaling actief is (hoofdstuk 12).
  Dit bepaalt welk geheugen het programma "ziet".
- Op sommige processoren: de **statusvlaggen** (was het resultaat nul?
  was er een overflow?) en de stand van de **floating-point-registers** (voor
  kommagetallen en vectorberekeningen).

Alles bij elkaar is dat een paar honderd tot een paar duizend bytes, afhankelijk
van de processor. Niet veel - maar het moet precies kloppen.

## De wissel in drie stappen

Een context switch werkt in drie stappen:

```text
Stap 1: Sla de context van A op
+----------+                   +-----------+
| Processor|  --- opslaan ---> | Geheugen  |
| (draait  |   registers,     | "Context  |
|  A)      |   PC, SP, ...    |  van A"   |
+----------+                   +-----------+

Stap 2: Laad de context van B
+-----------+                  +----------+
| Geheugen  |  --- laden --->  | Processor|
| "Context  |   registers,    | (klaar    |
|  van B"   |   PC, SP, ...   |  voor B)  |
+-----------+                  +----------+

Stap 3: Spring naar B
+----------+
| Processor|  --- verder met B's instructie
| (draait  |
|  B)      |
+----------+
```

Dat is het hele idee: bewaar het briefje van A, pak het briefje van B, en lees
het in. De processor "vergeet" A en "wordt" B.

## Wanneer gebeurt een context switch?

Een contextwisseling kan op drie momenten gebeuren:

1. **Vrijwillig**: programma A doet een syscall die zegt "ik wacht op iets"
   (een bestand lezen, slapen). De kernel schakelt naar B terwijl A wacht.

2. **Door de timer**: de timer-interrupt gaat af (hoofdstuk 11). De kernel
   onderbreekt A en geeft de beurt aan B. Dit heet **preemptie** (van het
   Engelse *preemption*: het recht om iemand te onderbreken).

3. **Bij een fout**: programma A doet iets verkeerds (een page fault die niet
   opgelost kan worden). De kernel stopt A en schakelt naar B.

In alle drie de gevallen is het mechanisme hetzelfde: context opslaan, andere
context laden.

## Wat er precies wordt opgeslagen

Laten we het concreet maken. Bij een contextwisseling op RISC-V worden onder
andere deze dingen bewaard:

```text
Context-blok van een taak (vereenvoudigd):
+----------+-----------------+
| ra       | terugkeeradres  |
| sp       | stack pointer   |
| s0 - s11 | bewaarde regs   |  <- 12 registers die een functie
|          |                 |     moet beschermen
| sepc     | program counter |  <- waar was het programma?
| sstatus  | statusregister  |  <- kernel-stand of gebruiker?
|          |                 |     interrupts aan of uit?
+----------+-----------------+
  + floating-point-registers (als die in gebruik zijn)
  + de paginatabel-pointer (satp op RISC-V, CR3 op x86, TTBR0 op ARM64)
```

De kernel heeft voor elke taak zo'n blok in het geheugen. Als taak A
wordt onderbroken, schrijft de kernel de huidige registerwaarden in het blok
van A. Daarna leest het de waarden uit het blok van B en zet ze in de
registers. De speciale instructie die de processor terug naar gebruikers-stand
brengt (`sret`/`eret`/`iret`) laadt de program counter en de status, en
programma B draait alsof er nooit iets anders was.

## De paginatabel wisselen

Een cruciaal onderdeel is het wisselen van de **paginatabel-pointer**. Elk
programma heeft zijn eigen paginatabel (hoofdstuk 12), en die bepaalt welk
geheugen het "ziet". Als de kernel van A naar B schakelt, moet hij het register
overzetten dat naar de paginatabel wijst:

- **RISC-V**: het `satp`-register.
- **ARM64**: het `TTBR0_EL1`-register.
- **x86-64**: het `CR3`-register.

Na het omzetten van dit register ziet de processor ineens een compleet ander
geheugenlandschap. De adressen die A gebruikte zijn weg; de adressen van B
zijn er.

## De kosten van een context switch

Een context switch is niet gratis. Er zijn drie soorten kosten:

### 1. Instructies

Het opslaan en laden van alle registers kost instructies. Op een moderne
processor zijn dat er ruwweg 50 tot 200, afhankelijk van hoeveel registers er
zijn en of je floating-point-registers meeneemt. Dat klinkt als weinig, maar het
gebeurt duizenden keren per seconde.

### 2. TLB-flush

Weet je nog de TLB uit hoofdstuk 12? Die snelle cache van geheugenvertalingen?
Als je van paginatabel wisselt, kloppen die vertalingen niet meer - ze horen bij
het vorige programma.

De eenvoudige oplossing: de TLB leegmaken (dat heet **flushen**). Maar dat
is duur, want na het flushen moet elke geheugenvertaling opnieuw worden
opgezocht in de paginatabel.

De slimmere oplossing: elke vertaling in de TLB een **label** geven (een
**ASID**, Address Space Identifier). De vertalingen van A krijgen label 1, die
van B label 2. Bij het wisselen hoeft de TLB niet leeg, want de processor weet
welke vertalingen bij welk programma horen. In rheo-os wordt dit gebruikt via
`paging_activate` in `kernel/src/arch/<isa>/paging.rs`.

### 3. Cache-effecten

De processor heeft caches: snel geheugen waar recent gebruikte data en
instructies in zitten. Na een switch werkt programma B met *zijn eigen* data en
instructies, die waarschijnlijk nog niet in de cache zitten. De eerste
geheugentoegangen van B zijn daarom langzamer. Dit heet een **cold cache**.

## Waarom een snelle context switch belangrijk is

Al deze kosten tellen op. Als een switch duur is, kun je niet vaak wisselen.
En als je niet vaak wisselt, reageren programma's traag: je drukt op een toets
en er gebeurt even niks.

Daarom is het een doel van elk OS om de context switch zo snel mogelijk te
maken. Minder registers opslaan als het kan (alleen de registers die de functie
echt verandert). TLB-labels gebruiken in plaats van flushen. Floating-point
alleen opslaan als het programma het echt gebruikt.

In rheo-os worden context switches gemeten in **instructies** in plaats van
in tijd, via QEMU's icount-modus. Zo is het resultaat herhaalbaar en niet
afhankelijk van hoe snel je computer is. De test `bench_core` meet dit; de
strand-runtime (het interne uitvoeringsmodel van rheo-os) schakelt in ongeveer
150 instructies, wat overeenkomt met een paar nanoseconden op echte hardware.

## Context switches in rheo-os

In rheo-os werkt het zo:

- De daadwerkelijke wissel-assembly (registers opslaan en laden) staat in
  `kernel/arch/<isa>/context_switch.S` - een apart bestand per ISA, want dit is
  een van de plekken waar assembly nodig is.
- De aansturing vanuit Rust zit in `kernel/src/user.rs`. De functie
  `switch_native_cell` is het centrale punt: die wisselt de FP/SIMD-registers
  *en* de paginatabel in een keer.
- De scheduler (`kernel/src/sched/`) beslist *wie* de beurt krijgt; de
  context-switch-code voert het uit.
- De trap frame (`TrapFrame` in `kernel/src/arch/<isa>/mod.rs`) bewaart de
  registers van het onderbroken programma, zodat de kernel ze later kan
  herstellen.

```text
Wie doet wat:

Timer-interrupt
     |
     v
Trap handler (assembly)   <- slaat registers op in de trap frame
     |
     v
Scheduler (Rust)          <- kiest de volgende taak
     |
     v
switch_native_cell (Rust) <- wisselt FP/SIMD + paginatabel
     |
     v
Context switch (assembly) <- laadt registers van de nieuwe taak
     |
     v
Terugkeer (sret/eret/iret) <- de nieuwe taak draait
```

## De vergelijking: een estafetteloop

Een context switch is als een estafetteloop. Loper A rent met het stokje (de
processor). Als het tijd is om te wisselen:

1. Loper A stopt en legt zijn positie, snelheid en richting vast (context
   opslaan).
2. Het stokje gaat naar loper B.
3. Loper B pakt zijn eigen positie en richting op (context laden).
4. Loper B rent verder.

De baan (de processor) is er maar een. De trucs zijn: het wisselen zo snel doen
dat het publiek (de gebruiker) denkt dat iedereen tegelijk rent, en niks
kwijtraken bij de overdracht.

## Samenvatting

- Een **context** is alles wat een taak uniek maakt: registers, program
  counter, stack pointer, paginatabel-pointer, en floating-point-staat.
- Bij een **context switch** slaat de kernel de context van de huidige taak op
  en laadt de context van de volgende.
- De kosten zijn: instructies voor het opslaan/laden, een mogelijke TLB-flush,
  en koude caches.
- **ASID's** (labels in de TLB) maken de switch sneller door het flushen te
  vermijden.
- De context-switch-code in rheo-os staat in
  `kernel/arch/<isa>/context_switch.S` en wordt aangestuurd door
  `switch_native_cell` in `kernel/src/user.rs`.

## Oefeningen

1. Noem minstens vijf dingen die tot de "context" van een draaiend programma
   horen.
2. Waarom is het wisselen van de paginatabel-pointer (zoals `satp` of `CR3`)
   een essentieel onderdeel van de context switch?
3. Leg uit waarom een TLB-flush een context switch duurder maakt. Hoe helpen
   ASID's daarbij?
4. De timer-interrupt gaat af. Beschrijf in je eigen woorden de stappen van
   "interrupt komt binnen" tot "een ander programma draait."
5. Stel dat een context switch 200 instructies kost en de processor 1 miljard
   instructies per seconde kan uitvoeren. Hoeveel context switches per seconde
   kun je dan doen? En hoeveel procent van de processortijd gaat verloren als
   je 10.000 keer per seconde wisselt?

Door naar de [woordenlijst](woordenlijst.md) of terug naar de
[inhoudsopgave](README.md).
