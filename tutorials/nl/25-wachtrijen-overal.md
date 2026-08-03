# Hoofdstuk 25 - Wachtrijen overal: alles is een wachtrij

Als je lang genoeg naar een besturingssysteem kijkt, zie je overal hetzelfde
patroon opduiken: de **wachtrij**. De scheduler heeft een wachtrij van taken
die aan de beurt willen. De schijf heeft een wachtrij van lees- en
schrijfopdrachten. De netwerkkaart heeft een wachtrij van pakketjes. De
toetsenbordinvoer staat in een wachtrij. Zelfs de manier waarop het OS met
zijn eigen gebruikersprogramma's praat, gaat via een wachtrij.

In dit hoofdstuk leer je waarom wachtrijen zo populair zijn, welke vormen ze
aannemen, en hoe ze hele systemen bij elkaar houden.

## Wat is een wachtrij?

Een **wachtrij** (Engels: *queue*) werkt precies zoals de rij bij de kassa in
de supermarkt. Wie het eerst aansluit, wordt het eerst geholpen. Dat heet
**FIFO**: First In, First Out. Het eerste dat erin gaat, komt er het eerst
weer uit.

```text
invoer ->  [ A | B | C | D ]  -> uitvoer
            ^                     ^
         achterkant             voorkant
         (hier komt             (hier wordt
          nieuw werk erin)       werk eruit gehaald)
```

Het mooie van een wachtrij: de **producent** (degene die werk erin stopt)
hoeft niet te weten wie het eruit haalt, en de **consument** (degene die het
eruit haalt) hoeft niet te weten wie het erin stopte. Ze praten niet met
elkaar; ze praten alleen met de wachtrij.

## De drie vormen die steeds terugkomen

Niet alle wachtrijen zijn gelijk. In een besturingssysteem kom je drie vormen
steeds opnieuw tegen:

### 1. De gewone FIFO-wachtrij

Eerst erin, eerst eruit. Simpel en eerlijk. Voorbeelden:

- De toetsenbordinvoer: de eerste toets die je aanslaat, wordt het eerst
  verwerkt.
- Een printerwachtrij: wie het eerst een document instuurt, wordt het eerst
  afgedrukt.
- De netwerkkaart: pakketjes worden verstuurd in de volgorde waarin ze
  binnenkomen.

### 2. De prioriteitswachtrij

Niet alles is even belangrijk. Een **prioriteitswachtrij** (priority queue)
haalt niet het oudste item eruit, maar het *belangrijkste*. Denk aan de
spoedeisende hulp in een ziekenhuis: een hartaanval gaat voor een gebroken
teen, ongeacht wie er eerder was.

Voorbeelden:

- De scheduler: een draad met een dringend deadline krijgt voorrang op een
  draad die "ooit een keer" mag draaien.
- Interrupts: een klokinterrupt kan belangrijker zijn dan een
  toetsenbordinterrupt.

In rheo-os gebruikt de scheduler een **EEVDF-wachtrij** met deadlines
(`kernel/src/sched/vcore.rs`). Taken met een eerder deadline worden eerder
gepland.

### 3. De work-stealing wachtrij

Stel je voor: vier kassa's in de supermarkt. Kassa 1 heeft een lange rij,
kassa 2 is bijna leeg. Bij een **work-stealing queue** mag kassa 2 werk
"stelen" uit de rij van kassa 1. Zo blijft iedereen bezig en wordt niemand
onnodig opgehouden.

Dit patroon wordt gebruikt als je meerdere processorcores hebt. Elke core
heeft zijn eigen werklijst, maar als een core niks te doen heeft, pakt hij
een taak van een drukke buurman.

In rheo-os kun je dit zien in de multi-core celplaatsing (`kernel/src/smp.rs`):
een core die klaar is met zijn eigen werk, kan een nog niet gestarte cel van
een andere core overnemen.

## Backpressure: als de wachtrij vol raakt

Wat als de producent sneller is dan de consument? Dan groeit de wachtrij. En
als de wachtrij een maximale grootte heeft (en dat heeft hij bijna altijd),
dan raakt hij vol.

Dat klinkt als een probleem, maar eigenlijk is het een **signaal**. Het
vertelt de producent: "Rustig aan, de consument kan het niet bijhouden." Dit
heet **backpressure** (tegendruk).

Vergelijk het met een water-pijpleiding. Als de afvoer verstopt raakt, stijgt
het water in de pijp. Dat stijgende water is het signaal dat er iets mis is
stroomafwaarts.

Er zijn verschillende manieren om met backpressure om te gaan:

- **Wachten.** De producent stopt tot er weer ruimte is. Simpel, maar dan
  staat de producent stil.
- **Weigeren.** De producent krijgt een foutmelding terug: "vol, probeer
  later." De producent beslist zelf wat te doen.
- **Wegooien.** Het oudste of minst belangrijke item wordt weggegooid om
  ruimte te maken. Dat klinkt grof, maar bij videostreaming gooi je liever een
  oud frame weg dan dat het beeld bevriest.

Het allerbelangrijkste: een volle wachtrij is **geen fout**, het is informatie.
Een systeem dat backpressure negeert, loopt uiteindelijk vast of raakt door
zijn geheugen heen.

## Wachtrijen als lijm tussen onderdelen

De echte kracht van wachtrijen is dat ze onderdelen van je systeem **los-
koppelen**. De producent hoeft niets te weten over de consument, en omgekeerd.
Ze hoeven niet eens op hetzelfde moment te draaien.

```text
+-------------+     wachtrij      +-------------+
|  netwerk-   | --> [ pakket  ] -->|  TCP-laag   |
|  kaart      |    [ pakket  ]    |             |
+-------------+    [ pakket  ]    +-------------+
                        |
                   ontkoppeling:
                   de netwerkkaart weet
                   niets van TCP, en TCP
                   weet niets van de
                   netwerkkaart
```

Dit is hetzelfde idee als een brievenbus: de postbode hoeft niet te weten
wanneer jij je post leest, en jij hoeft niet thuis te zijn als de postbode
komt. De brievenbus is de wachtrij.

In een besturingssysteem zie je dit patroon overal:

- Tussen het toetsenbord (producent) en de shell (consument).
- Tussen een programma (producent) en de schijf (consument).
- Tussen twee programma's die via een pipe praten.

## Wachtrijen in rheo-os

In rheo-os is de **queue-pair** het hart van de communicatie tussen een cel
(een gebruikersprogramma) en de kernel. Dit is een paar van twee ringen: een
**submission queue** (SQ, opdrachten van de cel naar de kernel) en een
**completion queue** (CQ, antwoorden van de kernel naar de cel).

De cel stopt een opdracht in de SQ, drukt op de deurbel (`SYS_DOORBELL`), en
de kernel verwerkt het. Als het klaar is, verschijnt het antwoord in de CQ.
De cel en de kernel werken via gedeeld geheugen, zonder data te kopieren.

```text
+--------+                          +--------+
|  cel   |  --[opdracht]-->  SQ  -->| kernel |
|        |  <--[antwoord]-- CQ  <--|        |
+--------+                          +--------+
```

De definities van de opdrachten en antwoorden staan in `abi/src/lib.rs`. De
ringlogica staat in `kernel/src/queue/mod.rs`. Elke opdracht heeft een
**opcode** (wat moet er gebeuren: lezen, schrijven, echo) en een **status**
(is het gelukt of niet).

Dit is hetzelfde patroon als de NVMe-schijf uit het vorige hoofdstuk. De
wachtrij is de universele taal.

## Samenvatting

- Een **wachtrij** is een rij waar items aan de achterkant ingaan en aan de
  voorkant worden verwerkt (FIFO).
- De drie vormen die steeds terugkomen: de **FIFO-wachtrij** (eerlijk, op
  volgorde), de **prioriteitswachtrij** (het belangrijkste eerst), en de
  **work-stealing queue** (een vrije werker pakt werk van een drukke buurman).
- **Backpressure** is het signaal dat een wachtrij vol raakt. Het is
  informatie, geen fout.
- Wachtrijen **ontkoppelen** producent en consument: ze hoeven niets van
  elkaar te weten.
- In rheo-os is de **queue-pair** (SQ + CQ) het hart van de communicatie,
  gedefinieerd in `abi/src/lib.rs` en uitgevoerd in `kernel/src/queue/mod.rs`.

## Oefeningen

1. Noem drie plekken in een besturingssysteem waar een wachtrij wordt
   gebruikt. Zeg bij elk wie de producent en wie de consument is.
2. Wat is het verschil tussen een FIFO-wachtrij en een prioriteitswachtrij?
   Bedenk een situatie waarin je per se een prioriteitswachtrij nodig hebt.
3. Leg backpressure uit met een vergelijking uit het dagelijks leven (niet de
   waterpijp uit de tekst).
4. Bekijk de opcodes in `abi/src/lib.rs` (zoek naar `OP_READ`, `OP_WRITE`,
   etc.). Hoeveel verschillende soorten opdrachten kan een cel via de queue
   sturen?
5. Waarom is het handig dat de producent en de consument niets van elkaar
   hoeven te weten? Wat zou er misgaan als ze wel aan elkaar gekoppeld waren?
