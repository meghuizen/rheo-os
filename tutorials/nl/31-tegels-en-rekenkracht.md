# Hoofdstuk 31 - Tegels en rekenkracht: GPU, NPU, FPGA en meer

In de vorige hoofdstukken zag je hoe de processor (CPU) werk verdeelt, geheugen
beheert en programma's laat samenwerken. Maar sommige berekeningen - denk aan
kunstmatige intelligentie, 3D-graphics of grote wetenschappelijke simulaties -
zijn zo zwaar dat zelfs de snelste CPU het niet redt. Daarvoor bestaan speciale
chips: GPU's, NPU's, TPU's en FPGA's. Ze zijn heel verschillend van buiten,
maar van binnen doen ze allemaal hetzelfde: ze hakken een groot probleem in
kleine blokjes en verwerken die blokjes massaal tegelijk. Die blokjes heten
**tegels** (Engels: *tiles*).

## Wat is een tegel?

Stel je een groot vel ruitjespapier voor met duizenden vakjes. Je moet elk vakje
inkleuren. Je kunt dat een voor een doen (de CPU-manier), of je kunt het papier
in kleine vierkantjes knippen en elk vierkantje aan een ander persoon geven. Zo'n
vierkantje is een **tegel**: een klein stukje data dat als eenheid wordt
verwerkt.

In de praktijk is een tegel meestal een rechthoekig blokje uit een **matrix**
(een tabel met getallen). Denk aan een matrix van 1024 bij 1024 getallen. Die
knip je in tegels van bijvoorbeeld 32 bij 32:

```text
Grote matrix (1024 x 1024)
+--------+--------+--------+---
| tegel  | tegel  | tegel  |
| 32x32  | 32x32  | 32x32  | ...
+--------+--------+--------+---
| tegel  | tegel  | tegel  |
| 32x32  | 32x32  | 32x32  | ...
+--------+--------+--------+---
|  ...   |  ...   |  ...   |
```

Elke tegel past in het snelle lokale geheugen van een rekenkerntje. Dat is de
truc: het blokje zit dicht bij de rekenkracht, zodat er geen tijd verloren gaat
aan wachten op het verre hoofdgeheugen.

## Waarom GPU's in tegels werken

Een **GPU** (Graphics Processing Unit) is ontworpen om heel veel simpele
berekeningen tegelijk te doen. Een CPU heeft 4 tot 16 krachtige kernen; een GPU
heeft *duizenden* kleine, simpele kernen.

Die duizenden kernen zijn georganiseerd als een piramide:

```text
GPU: grid -> blokken -> threads

+----- Grid (het hele werk) -----+
|                                |
| +-- Blok 0 --+ +-- Blok 1 --+ |
| | t0 t1 t2   | | t0 t1 t2   | |
| | t3 t4 t5   | | t3 t4 t5   | |
| +------------+ +------------+ |
|                                |
| +-- Blok 2 --+ +-- Blok 3 --+ |
| | t0 t1 t2   | | t0 t1 t2   | |
| | t3 t4 t5   | | t3 t4 t5   | |
| +------------+ +------------+ |
+--------------------------------+
```

Elk **blok** is een groep threads die samenwerken en snel geheugen delen. Elk
blok verwerkt een tegel. Het hele **grid** is de verzameling blokken die samen
het volledige probleem oplossen.

Vergelijk het met een restaurant. De CPU is een chefkok die ingewikkelde
gerechten een voor een bereid. De GPU is een keuken met honderd hulpkoks die
allemaal tegelijk een eenvoudig gerecht klaarmaken. Geen van de hulpkoks is zo
goed als de chef, maar samen zijn ze veel sneller als er honderd borden klaar
moeten.

## Systolische arrays: data stroomt als water door buizen

Een **systolische array** is een ander ontwerp dat je vindt in chips als
Google's **TPU** (Tensor Processing Unit) en Intel's **AMX** (Advanced Matrix
Extensions). Het woord "systolisch" komt uit de geneeskunde: het hartritme dat
bloed door het lichaam pompt.

Stel je een raster van rekeneenheden voor, als een dambord. Elke eenheid doet
precies een ding: **vermenigvuldigen en optellen** (een FMA: *fused
multiply-add*). De data stroomt er doorheen van links naar rechts en van boven
naar beneden, als water door een buizenstelsel:

```text
Systolische array (4x4 vereenvoudigd)

A-rij -->  [*+] --> [*+] --> [*+] --> [*+]
             |        |        |        |
A-rij -->  [*+] --> [*+] --> [*+] --> [*+]
             |        |        |        |
A-rij -->  [*+] --> [*+] --> [*+] --> [*+]
             |        |        |        |
A-rij -->  [*+] --> [*+] --> [*+] --> [*+]
             ^        ^        ^        ^
             |        |        |        |
           B-kolom  B-kolom  B-kolom  B-kolom

Elke [*+] vermenigvuldigt een A-element met een B-element
en telt het op bij het tussenresultaat dat erlangs stroomt.
```

Het geniale: elke eenheid leest zijn input van zijn buurman, niet uit het
hoofdgeheugen. De data beweegt als een golf door het raster. Na een paar
stappen stroomt er aan de onderkant een rij van het resultaat uit.

Dit is razendsnel voor **matrixvermenigvuldiging** (twee tabellen met getallen
maal elkaar): precies de berekening die neurale netwerken nodig hebben.

## FPGA: je bouwt je eigen hardware

Een **FPGA** (Field-Programmable Gate Array) is een chip waar *jij* de
bedrading bepaalt. Normaal is hardware vast: een GPU is een GPU, een CPU is
een CPU. Bij een FPGA programmeer je hoe de transistoren met elkaar verbonden
zijn. Je kunt er je eigen systolische array in bouwen, of een heel ander
ontwerp.

Vergelijk het met LEGO: andere chips zijn kant-en-klare bouwsets (een kasteel,
een vliegtuig), maar een FPGA is een bak losse steentjes waarmee je alles kunt
bouwen wat je wilt. Het nadeel: het is langzamer per bewerking dan een
speciaal ontworpen chip, en veel moeilijker om te programmeren.

FPGA's worden gebruikt waar flexibiliteit belangrijker is dan snelheid, of waar
de aantallen te klein zijn om een eigen chip te laten maken.

## NPU en TPU: speciale chips voor neurale netwerken

Een **NPU** (Neural Processing Unit) zit tegenwoordig in veel telefoons en
laptops. Een **TPU** is Google's variant, die in hun datacenters staat. Beiden
zijn geoptimaliseerd voor dezelfde berekening: **matrix maal matrix, gevolgd
door een wiskundige functie** (de "activatie", zoals "als het getal negatief is,
maak het nul"). Dat is precies wat een laag in een neuraal netwerk doet.

Ze zijn in feite systolische arrays met extra logica voor die activatiefuncties,
en met geheugen dat is ingericht om tegels zo snel mogelijk aan te voeren.

## Waarom alles op hetzelfde uitkomt

GPU, TPU, NPU, FPGA - vier heel verschillende chips, maar ze lossen allemaal
hetzelfde probleem op:

1. **Knip het werk in kleine blokjes** (tegels).
2. **Houd de data dicht bij de rekenkracht** (snel lokaal geheugen per blok).
3. **Verwerk zo veel mogelijk blokjes tegelijk** (massaal parallel).

```text
                   Alle wegen leiden naar tegels

    GPU              TPU/NPU            FPGA
    duizenden        systolische        zelf te
    kleine kernen    array              programmeren
       \                |                /
        \               |               /
         v              v              v
     +-------------------------------+
     | Hetzelfde patroon:            |
     | data in tegels knippen,       |
     | tegels dicht bij rekenkracht, |
     | massaal parallel verwerken    |
     +-------------------------------+
```

Dit is geen toeval. Het komt door een fundamentele eigenschap van wiskunde en
hardware: een klein blokje data past in snel geheugen, en als de blokjes
onafhankelijk van elkaar zijn, kun je ze allemaal tegelijk uitrekenen.

## Van GEMM naar aandacht: tegels in de praktijk

De belangrijkste tegelbewerking is **GEMM** (General Matrix Multiply): twee
matrices met elkaar vermenigvuldigen. Bijna alles in kunstmatige intelligentie
en 3D-graphics komt erop neer.

Een stap verder is **FlashAttention**: de kern van moderne taalmodellen (zoals
ChatGPT). In plaats van de hele matrix in het geheugen te houden, verwerkt
FlashAttention de invoer in blokjes - tegels - en berekent het resultaat
*per tegel*, zodat het geheugengebruik beperkt blijft. De wiskundige truc
heet "online softmax": je houdt een lopend gemiddelde bij terwijl je tegel
voor tegel door de data gaat.

In rheo-os vind je dit terug:

- `librheo/src/tile/kernels.rs` - de rekenkernen voor GEMM (int8 naar i32).
- `librheo/src/tile/attn.rs` - FlashAttention 2 en 3.
- `librheo/src/tile/fmath.rs` - wiskundige functies (`exp2f`, `expf`) die de
  softmax nodig heeft.

## Hoe het past in het wachtrijmodel

In hoofdstuk 32 leer je dat alles in een OS een wachtrij is. Tegels passen
daar precies in. Een **tegelprogramma** is een lijst van stappen met
afhankelijkheden: "bereken tegel A, bereken tegel B, en als beide klaar zijn,
tel de resultaten op." Dat is een **afhankelijkheidsgrafiek** (dependency
graph): een tekening van wat na wat komt.

In rheo-os wordt zo'n grafiek als een pakketje aangeboden aan de kernel via de
wachtrij:

```text
Cel (je programma)
  |
  |  "Hier is mijn tegelprogramma"
  v
+---------------------------+
| Wachtrij (queue pair)     |
| OP_GRAPH_SUBMIT           |
+---------------------------+
  |
  v
Kernel: engine.rs + graph.rs
  |
  |  Controleert de afhankelijkheden,
  |  voert de stappen uit op de
  |  beschikbare engine (CPU nu,
  |  GPU/NPU later)
  v
+---------------------------+
| Resultaat terug via       |
| de completie-wachtrij     |
+---------------------------+
```

De code hiervoor staat in `kernel/src/engine.rs` (het register van beschikbare
engines) en `kernel/src/graph.rs` (het uitvoeren van de grafiek). De cel hoeft
niet te weten of het werk op een CPU of een GPU draait - de wachtrij en de
engine regelen dat. Dat is de kracht van het model: *een programma, elke
engine*.

## Samenvatting

- Een **tegel** (tile) is een klein blokje data uit een groter geheel,
  verwerkt als eenheid.
- Een **GPU** heeft duizenden kleine kernen die elk een tegel tegelijk
  verwerken, georganiseerd in blokken en een grid.
- Een **systolische array** (in TPU's en AMX) laat data als water door een
  raster van vermenigvuldig-en-optel-eenheden stromen.
- Een **FPGA** is programmeerbare hardware waar je je eigen ontwerp in bouwt.
- Een **NPU/TPU** is gespecialiseerd in de matrixberekeningen van neurale
  netwerken.
- Al deze chips convergeren op hetzelfde patroon: knip in tegels, houd data
  dichtbij, verwerk massaal parallel.
- **GEMM** (matrixvermenigvuldiging) en **FlashAttention** zijn de twee
  belangrijkste tegeloperaties in de praktijk.
- In een OS is een tegelprogramma een afhankelijkheidsgrafiek die via de
  wachtrij aan een engine wordt aangeboden.

## Oefeningen

1. Leg in je eigen woorden uit waarom een GPU sneller is dan een CPU voor het
   inkleuren van een miljoen pixels, maar langzamer voor het uitrekenen van
   een ingewikkelde formule met veel if/else-vertakkingen.
2. Teken een systolische array van 3 bij 3 en volg stap voor stap hoe de
   getallen erdoorheen stromen als je matrix A = [[1,2],[3,4]] vermenigvuldigt
   met matrix B = [[5,6],[7,8]].
3. Waarom is het handig dat een tegelprogramma als een grafiek wordt
   aangeboden in plaats van als een lijst stappen? Hint: denk aan parallellisme.
4. Bekijk `librheo/src/tile/kernels.rs` in rheo-os. Wat voor bewerking doet
   de GEMM-kernel precies? Welk datatype gebruikt hij (int8, f32, iets anders)?
5. Bedenk een probleem uit het dagelijks leven dat je zou kunnen opdelen in
   tegels. Beschrijf hoe je het zou aanpakken met het tegel-patroon.

Door naar [hoofdstuk 32](32-het-wachtrijmodel.md): hoe alle stukken van dit
boek samenkomen in een wachtrijmodel.
