# Hoofdstuk 21 - Meerdere processoren tegelijk: SMP en cache-coherentie

Vroeger had een computer een processor. Nu heeft bijna elke computer er
meerdere: je telefoon heeft er vier of acht, een server soms honderdtwintig.
Maar een besturingssysteem moet die cores wel ontdekken, opstarten en aan
het werk zetten. En er is een lastig probleem: als twee cores allebei naar
hetzelfde stukje geheugen schrijven, wie wint er dan? In dit hoofdstuk leer
je hoe dat werkt.

## SMP: alle cores zijn gelijk

**SMP** staat voor *Symmetric Multi-Processing*. Het woord "symmetrisch"
betekent hier: alle cores zijn gelijkwaardig. Elke core kan dezelfde
instructies uitvoeren en bij hetzelfde geheugen. Er is geen "baas-core" en
"knecht-core" - ze zijn gelijk.

(In de praktijk is er wel een eerste core die de computer opstart, de
**boot-core** of **BSP** (Bootstrap Processor). Maar nadat die de andere
cores heeft wakker gemaakt, zijn ze allemaal gelijk.)

## Hoe ontdekt en start een OS meerdere cores?

Het opstarten van extra cores gaat in drie stappen:

### 1. Ontdekken: hoeveel cores zijn er?

De firmware (het programmaatje dat voor het OS draait) vertelt hoeveel
cores er zijn. Elke ISA doet dit anders:

- **x86-64**: via de ACPI-tabellen (een soort inventarislijst die de
  firmware achterlaat in het geheugen).
- **ARM64**: via PSCI (een interface waarmee je cores kunt aanzetten) of
  een device tree.
- **RISC-V**: via de device tree (een beschrijving van de hardware als een
  boomstructuur in het geheugen).

### 2. Wakker maken: start de andere cores

De boot-core stuurt een seintje naar elke slapende core:

- **x86-64**: het beroemde **INIT-SIPI-SIPI**-ritueel. De boot-core stuurt
  drie speciale berichten via de APIC (de interrupt-controller), en de
  slapende core begint te draaien op een afgesproken adres.
- **ARM64**: een PSCI `CPU_ON`-aanroep. De boot-core vraagt de firmware om
  een core te starten.
- **RISC-V**: een SBI `HSM`-aanroep. Vergelijkbaar met ARM's aanpak.

### 3. Aan het werk: geef elke core iets te doen

Als een core wakker wordt, heeft hij nog niets om te doen. Het OS moet hem
een stuk werk geven. Dat kan op twee manieren:

- **Een werkwachtrij**: alle beschikbare taken staan in een rij. Elke core
  die vrij is, pakt de volgende taak. Wie het eerst komt, die het eerst
  maalt.
- **Toewijzing**: het OS wijst een specifieke taak toe aan een specifieke
  core.

In rheo-os werkt het als een werkwachtrij: `smp::place_cells` publiceert
een set beschikbare cellen en elke core pakt er een. Dat staat in
`kernel/src/smp.rs`.

## Cache-coherentie: het verborgen probleem

Nu komt het lastigste deel. Elke core heeft zijn eigen **cache**: een klein,
supersnel geheugen vlak bij de core waar kopietjes van veelgebruikte data
in zitten. (We bespraken caches in hoofdstuk 2 als "een la in het bureau
naast je werkplek, dichter bij dan de kast in de gang".)

Het probleem: als core 0 een waarde in zijn cache verandert, heeft core 1
misschien nog de *oude* waarde in zijn eigen cache. Zonder afspraken leest
core 1 verkeerde data.

```text
  Core 0 cache:  count = 43  (net bijgewerkt)
  Core 1 cache:  count = 42  (oud kopietje!)
  Geheugen:      count = 42  (ook nog oud)

  Core 1 leest count --> krijgt 42 --> FOUT
```

De oplossing is een **protocol** dat alle caches met elkaar laat praten.
Het bekendste protocol heet **MESI**.

## MESI: vier toestanden van een cache-regel

MESI is een afkorting van vier toestanden waarin een cache-regel kan zijn:

- **M (Modified)**: "Ik heb dit veranderd en niemand anders heeft het."
  De cache-regel is nieuwer dan het geheugen. Deze core is de enige eigenaar.

- **E (Exclusive)**: "Ik heb dit als enige, maar ik heb het niet veranderd."
  De cache-regel is gelijk aan het geheugen, maar alleen deze core heeft
  een kopie.

- **S (Shared)**: "Ik heb dit, maar anderen misschien ook."
  De cache-regel is gelijk aan het geheugen. Meerdere cores kunnen
  dezelfde kopie hebben.

- **I (Invalid)**: "Dit is ongeldig, ik moet het opnieuw ophalen."
  De cache-regel is waardeloos; de core moet het geheugen of een andere
  core raadplegen.

```text
  Voorbeeld: core 0 schrijft, core 1 leest daarna

  Stap 1: Core 0 leest X      --> E (exclusive, alleen core 0 heeft het)
  Stap 2: Core 1 leest X      --> S (shared, beiden hebben het)
           Core 0 gaat ook     --> S
  Stap 3: Core 0 schrijft X   --> M (modified, core 0 is eigenaar)
           Core 1 wordt        --> I (invalid, moet opnieuw ophalen)
  Stap 4: Core 1 leest X      --> Core 0 stuurt zijn M-kopie
           Core 0 gaat naar    --> S
           Core 1 gaat naar    --> S
```

Dit alles gebeurt automatisch in de hardware. Als programmeur hoef je het
protocol niet zelf aan te sturen, maar je moet wel *weten* dat het bestaat,
want het heeft gevolgen voor de snelheid van je code.

## Gedeeld geheugen versus berichten

Er zijn twee manieren waarop cores kunnen samenwerken:

### Gedeeld geheugen (shared memory)

Beide cores lezen en schrijven naar hetzelfde stukje geheugen. Ze gebruiken
atomaire operaties en vergrendelingen (zie hoofdstuk 20 en 22) om dat
veilig te doen.

**Voordeel**: snel als de data klein is, want er wordt niets gekopieerd.
**Nadeel**: lastig om correct te doen. Vergrendelingen, geheugenordening en
race conditions liggen op de loer.

### Berichten (message passing)

Elke core heeft zijn eigen data. Om iets te delen, stuurt de ene core een
bericht naar de andere. De ontvangende core krijgt een kopie.

**Voordeel**: geen gedeelde data, dus geen vergrendelingsproblemen.
**Nadeel**: het kopieren kost tijd, vooral bij grote data.

In de praktijk gebruiken besturingssystemen allebei. rheo-os doet dat
ook: de queue-pair ABI (het wachtrij-systeem waarmee cellen praten) is
berichten-gebaseerd, terwijl de frame-allocator (`mm::frames`) gedeeld
geheugen met een spinlock gebruikt.

## Per-CPU data: sommige dingen deel je niet

Sommige data hoort bij een core en bij geen andere. Denk aan:

- De teller van hoeveel keer deze core een timer-interrupt heeft gehad.
- De FPU-registers die bewaard zijn voor het programma op deze core.
- De "welke taak draai ik nu?"-aanwijzer.

Die data zet je in een **per-CPU structuur**: elke core heeft zijn eigen
kopie, geindiceerd op zijn eigen nummer. Omdat niemand anders erbij kan,
heb je geen vergrendeling nodig. Dat is sneller en simpeler.

In rheo-os heet die structuur `PerCpu<T>` (in `kernel/src/smp.rs`). De
functie `cpu_index()` vertelt elke core welk nummer hij heeft, zodat hij
bij zijn eigen data kan.

```text
  Per-CPU data (PerCpu<T>):

  +------------------+------------------+------------------+
  |    Core 0        |    Core 1        |    Core 2        |
  |  eigen teller    |  eigen teller    |  eigen teller    |
  |  eigen taak      |  eigen taak      |  eigen taak      |
  +------------------+------------------+------------------+

  Elke core leest en schrijft alleen zijn eigen vak.
  Geen vergrendeling nodig!
```

## rheo-os en SMP

In rheo-os staan de per-ISA opstartcode voor secondaire cores in:

- `kernel/arch/x86_64/smp.S` - de 16-bit trampoline voor INIT-SIPI-SIPI
- `kernel/arch/aarch64/psci.S` - de PSCI `CPU_ON`-aanroep
- `kernel/arch/riscv64/smp.S` - de SBI HSM-aanroep

Het draagbare deel staat in `kernel/src/smp.rs`: de `SpinLock<T>`, de
`PerCpu<T>`, de `cpu_index()`-functie en de code die de ISA-laag vraagt om
een secondaire core te starten.

Het `smp`-testkerneltje bewijst dat twee (en later vier) cores tegelijk
draaien door ze allebei naar een gedeelde teller te laten schrijven en te
controleren dat het resultaat klopt.

## Samenvatting

- **SMP** (Symmetric Multi-Processing): alle cores zijn gelijkwaardig.
- Het OS ontdekt cores via firmware, maakt ze wakker met een per-ISA
  mechanisme, en geeft ze werk.
- Elke core heeft een eigen **cache**. Het **MESI-protocol** houdt die
  caches met elkaar in de pas.
- Cores werken samen via **gedeeld geheugen** (snel, lastig) of
  **berichten** (veilig, kopieert).
- **Per-CPU data** is data die bij een core hoort en nooit gedeeld wordt.
  Dat vermijdt vergrendeling.

## Oefeningen

1. Wat betekent "symmetrisch" in SMP?
2. Beschrijf in je eigen woorden wat er misgaat als twee cores dezelfde
   variabele in hun cache hebben en er een schrijft zonder MESI.
3. Core 0 heeft een cache-regel in toestand M (Modified). Core 1 wil
   diezelfde data lezen. Wat gebeurt er volgens het MESI-protocol?
4. Geef een voorbeeld van data die je per-CPU zou maken, en leg uit
   waarom gedeeld beter *niet* is.
5. Zoek in `kernel/src/smp.rs` de definitie van `SpinLock`. Hoeveel velden
   heeft die struct?

Door naar [hoofdstuk 22](22-vergrendelen.md): hoe je ervoor zorgt dat
maar een core tegelijk bij gedeelde data kan.
