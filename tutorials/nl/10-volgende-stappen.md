# Hoofdstuk 10 - Wat nu? De weg naar een echt besturingssysteem

Je hebt bootloaders gebouwd voor drie processoren en een kernel in C gestart.
Dat is een enorme prestatie. Maar een echt besturingssysteem kan natuurlijk veel
meer dan tekst tonen. In dit laatste hoofdstuk krijg je de landkaart: wat komt er
allemaal nog, in welke volgorde, en waar je verder kunt leren.

Je hoeft dit niet allemaal morgen te kunnen. Zie het als een menukaart voor de
komende maanden of jaren.

## De landkaart: van "hallo" naar een echt OS

Ongeveer in deze volgorde bouwen mensen een besturingssysteem op. Elk stapje
bouwt voort op het vorige.

### 1. Netjes tekst in en uit (I/O)

Je kunt al tekst tonen. De volgende stap is tekst *lezen* van het toetsenbord of
de seriële poort, en netjes getallen kunnen tonen (bijvoorbeeld een getal als
"42" op het scherm). Zo kun je met je OS "praten".

### 2. Interrupts (onderbrekingen)

Tot nu toe controleert onze code steeds zelf of er iets gebeurd is (dat heet
**pollen**). Beter is: de hardware **onderbreekt** de processor als er iets is.
Zo'n onderbreking heet een **interrupt**. Denk aan een deurbel: je hoeft niet
elke minuut naar de deur te lopen; de bel gaat als er iemand is.

Interrupts heb je nodig voor: een toets die wordt ingedrukt, een tikkende klok
(**timer**), een netwerkpakketje dat binnenkomt. Dit is ook de basis voor het
laten afwisselen van programma's.

### 3. Geheugenbeheer en paging

Nu gebruikt onze kernel gewoon het hele geheugen. Een echt OS moet het geheugen
*verdelen* over programma's en ze *beschermen* tegen elkaar. Het gereedschap
daarvoor heet **paging**: het geheugen wordt in blokjes (pagina's) verdeeld, en
een tabel bepaalt welk programma bij welk blokje mag.

Paging is ook wat gebruikersprogramma's in hun eigen "speeltuin" houdt, zoals we
in hoofdstuk 1 bespraken.

### 4. Processen en scheduling

Een **proces** is een draaiend programma met zijn eigen geheugen. Meerdere
processen tegelijk laten draaien op één processor doe je met **scheduling**: heel
snel afwisselen wie de processor krijgt. Nu wordt je kernel echt een baas die
werk verdeelt.

### 5. Gebruikersruimte en systeemaanroepen

Tot nu toe draait alles in de machtige kernel-stand. De volgende grote stap is
programma's laten draaien in de veilige **gebruikers-stand**, die alleen via
**systeemaanroepen** (syscalls) bij de kernel mogen. Dit is het echte "kernel
versus gebruikersruimte" uit hoofdstuk 1.

### 6. Drivers voor apparaten

Een **driver** is code die met een apparaat praat: een schijf, een netwerkkaart,
een grafische kaart. Met een schijf-driver kun je bestanden bewaren; met een
netwerk-driver kun je online.

### 7. Bestandssysteem

Een **bestandssysteem** ordent de bytes op een schijf in mappen en bestanden met
namen. Zonder bestandssysteem is een schijf gewoon een enorme rij bytes.

### 8. En dan de grote wereld

Netwerk, meerdere processoren tegelijk (**multi-core**), grafische schermen, echte
programma's draaien... Dit gaat eindeloos door. Een besturingssysteem is nooit
"af".

## Dit project als voorbeeld

Je zit in de code van **rheo-os**, een echt besturingssysteem in aanbouw. Het is
geschreven in Rust en draait op precies de drie ISA's uit dit boek. Alles wat je
hierboven op de landkaart zag, kun je er in het echt bekijken:

- De processor-specifieke assembly staat in `kernel/arch/` - net het dunne
  laagje uit hoofdstuk 9, maar dan voor een echt OS.
- De ontwerpteksten staan in de map `docs/`. Die zijn voor gevorderden, maar
  bladeren mag altijd. Kijk bijvoorbeeld naar:
  - `docs/ARCHITECTURE.md` - het grote plaatje.
  - `docs/MEMORY.md` - hoe het geheugen wordt beheerd (paging).
  - `docs/SMP.md` - meerdere processoren tegelijk (multi-core).

Het is normaal als je daar nog lang niet alles van snapt. Zelfs profs zoeken veel
op. Zie het als een keuken waar je mag rondkijken terwijl je zelf nog leert
koken.

## Waar leer je verder?

- **OSDev Wiki** (osdev.org) - de bekendste verzameling uitleg over het bouwen van
  besturingssystemen. Veel is in het Engels; dat went snel.
- **De reference manuals van de ISA's** - de officiele handboeken van RISC-V, ARM
  en Intel. Je hoeft ze niet te lezen als een boek; je zoekt erin op wat je nodig
  hebt.
- **Kleine projecten van anderen** - zoek op "RISC-V bare metal" of "os in Rust"
  en lees hoe anderen het doen. Code lezen is een van de beste manieren om te
  leren.
- **Blijf bouwen.** Neem de landkaart hierboven en pak stap 1 (getallen tonen,
  toetsen lezen). Daarna stap 2. Klein beginnen, vaak proberen.

## Tips voor onderweg

- **Verander steeds maar één ding.** Werkt het niet meer, dan weet je meteen wat
  het was.
- **Lees foutmeldingen rustig.** Er staat bijna altijd een aanwijzing in.
- **Print veel.** Als je niet weet wat je code doet, laat het je vertellen: toon
  een getal of een woord om te zien hoe ver hij komt.
- **Bewaar wat werkt.** Gebruik `git` om je werk op te slaan, zodat je altijd
  terug kunt naar een versie die het deed.
- **Niet opgeven bij een crash.** Een crash is geen mislukking, het is
  informatie. Je bent aan het leren op het diepste niveau van de computer.

## Tot slot

Je begon dit boek misschien met het idee dat besturingssystemen magie waren. Nu
weet je: het zijn instructies, registers, adressen en een paar slimme ideeen, in
lagen op elkaar gestapeld. Je hebt de onderste laag met je eigen handen gelegd.

De rest is meer van hetzelfde: kleine, begrijpelijke stukjes, netjes op elkaar.
Ga bouwen. Maak dingen kapot. Leer. En veel plezier.

## Samenvatting

- Na de bootloader komen: I/O, interrupts, paging, processen, gebruikersruimte,
  drivers, bestandssystemen, en de grote wereld daarna.
- Elk stapje bouwt voort op het vorige; een OS is nooit "af".
- rheo-os in dit repository is een echt voorbeeld om in rond te kijken
  (`kernel/arch/`, `docs/`).
- Leer verder via OSDev Wiki, de ISA-handboeken en de code van anderen.
- Verander één ding tegelijk, lees foutmeldingen, en geef niet op bij een crash.

## Laatste oefening

1. Kies stap 1 van de landkaart (een getal netjes tonen) en probeer het te bouwen
   bovenop je kernel uit hoofdstuk 9. Tip: reken het getal om naar losse cijfers
   en toon die als tekens.
2. Schrijf op wat jij als volgende wilt bouwen, en waarom.
3. Sla je werk op met `git`, zodat je trots kunt terugkijken op waar je begon.

Dit was het laatste hoofdstuk. Terug naar de [inhoudsopgave](README.md).
