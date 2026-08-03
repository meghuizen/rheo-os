# Hoofdstuk 32 - Het wachtrijmodel: alles komt samen

Dit is het laatste hoofdstuk van dit boek. Je hebt bootloaders gebouwd,
interrupts leren kennen, geheugen beheerd, processen en threads begrepen,
netwerken verkend en zelfs GPU-tegels gezien. Nu zoomen we uit en kijken we
naar het grote plaatje. En dat plaatje is verrassend eenvoudig: **alles is een
wachtrij**.

## Het idee in een zin

Overal in een computer waar een stukje werk van de ene plek naar de andere
moet, staat een wachtrij. De processor, de schijf, de netwerkkaart, de GPU,
de scheduler - ze zijn allemaal verbonden door wachtrijen. Begrijp je hoe een
wachtrij werkt, dan begrijp je hoe een heel besturingssysteem in elkaar zit.

## Drie eigenschappen die overal terugkomen

Elke wachtrij in een computer heeft dezelfde drie eigenschappen:

### 1. Producent en consument

Er is altijd iemand die werk *in* de rij zet (de **producent**) en iemand die
werk *uit* de rij haalt (de **consument**). Vergelijk het met een bakkerij: de
bakker legt broden op de toonbank (producent), klanten nemen ze mee
(consument).

```text
Producent --> [ item | item | item ] --> Consument
```

### 2. Tegendruk (backpressure)

Wat als de consument langzamer is dan de producent? Dan loopt de rij vol. Er
moet een manier zijn om de producent te vertragen of te laten wachten. Dat
heet **backpressure** (tegendruk).

In een bakkerij: als de toonbank vol ligt, moet de bakker even stoppen. Bij een
netwerkkaart: als de ontvangstbuffer vol zit, worden nieuwe pakketjes geweigerd.
Bij TCP: de ontvanger vertelt de zender hoeveel ruimte hij nog heeft (het
"venster").

Zonder tegendruk loopt het systeem over en raakt data kwijt.

### 3. Bundelen (batching)

In plaats van elk item apart te verwerken, kun je ze **bundelen**: een stapeltje
tegelijk oppakken. Dat is efficienter, omdat het opstarten van een verwerking
vaak duur is (denk aan een context switch of een schijfoperatie).

In een restaurant: de ober brengt niet elk bord apart naar de keuken, maar wacht
tot hij een dienblad vol heeft. Bij een NVMe-schijf: je stuurt acht
leesopdrachten in een keer naar de controller en drukt dan een keer op de
deurbel.

## Een HTTP-verzoek: de reis door het hele systeem

Laten we een concreet voorbeeld volgen. Je typt een webadres in je browser. Wat
gebeurt er onder de motorkap, stap voor stap? Elke stap is een wachtrij.

```text
Je browser              (1) "Haal deze pagina op"
     |
     v
+-- Socket-buffer --+   (2) Applicatie schrijft data in de zendbuffer
|   [ HTTP GET .. ] |
+-------------------+
     |
     v
+-- TCP-stack -------+  (3) TCP knipt de data in segmenten,
|   [ seg | seg ]    |      nummert ze, berekent checksums
+--------------------+
     |
     v
+-- TX-ring ---------+  (4) De driver zet de pakketjes in de
|   [ pkt | pkt ]    |      verzend-wachtrij van de netwerkkaart
+--------------------+
     |
     v
=== De draad / het internet ===
     |
     v
+-- RX-ring ---------+  (5) De netwerkkaart van de server ontvangt,
|   [ pkt | pkt ]    |      legt de pakketjes in de ontvangst-ring
+--------------------+
     |
     v  (interrupt!)
+-- TCP-stack -------+  (6) TCP zet de segmenten in volgorde,
|   [ seg | seg ]    |      controleert de checksums
+--------------------+
     |
     v
+-- Socket-buffer --+   (7) De applicatie leest het verzoek
|   [ HTTP GET .. ] |
+-------------------+
     |
     v
  Webserver             (8) Verwerkt het verzoek, stuurt antwoord
     |                       ... en de hele keten loopt terug
     v
(dezelfde wachtrijen in omgekeerde richting)
```

Tel de wachtrijen: socket-buffer, TCP-segmentrij, TX-ring, RX-ring, weer
TCP, weer socket-buffer. Zes wachtrijen voor een reis heen. En bij elke
wachtrij gelden de drie eigenschappen: er is een producent en een consument,
er is tegendruk (TCP's venster, de ringgrootte), en er wordt gebundeld (meerdere
segmenten per doorgifte).

## Terugkijken: elk hoofdstuk was een wachtrij

Nu je dit patroon kent, kun je terugbladeren door het boek en het overal
herkennen:

- **Interrupts** (hoofdstuk 11): de interrupt-controller is een wachtrij van
  signalen. De hardware is de producent, de handler de consument. Prioriteiten
  bepalen de volgorde.

- **Geheugen** (hoofdstukken over paging en frames): de frame-allocator is een
  wachtrij van vrije geheugenblokken. Het OS vraagt er een (consument), en geeft
  hem later terug (producent).

- **Scheduling** (wie krijgt de processor): de **ready queue** is letterlijk een
  wachtrij van processen die willen draaien. De scheduler haalt er een uit en
  geeft die de processor.

- **Schijf-I/O** (NVMe, virtio-blk): een **submission queue** waar de driver
  opdrachten in zet, en een **completion queue** waar de schijf de resultaten
  neerlegt. Twee ringen die samenwerken.

- **Netwerk** (TCP, ARP, DNS): meerdere geneste wachtrijen van pakketjes,
  segmenten en berichten.

- **Tegels en GPU** (hoofdstuk 31): een tegelprogramma is een wachtrij van
  taken die aan de engine worden aangeboden via een grafiek.

- **Processen en IPC** (inter-process communication): twee processen die met
  elkaar praten via een pipe of een kanaal - een producent en een consument,
  met tegendruk als de buffer vol zit.

## De wachtrij in rheo-os: het queue pair

In rheo-os is het wachtrijmodel niet alleen een manier van denken, maar de
echte kern van het ontwerp. Elk programma (een **cel**) praat met de kernel
via een **queue pair**: een paar ringen in gedeeld geheugen.

```text
Cel (je programma)
  |
  |  Zet een opdracht in de submission queue (SQ)
  |
  v
+------------------------------------+
| Queue Pair (gedeeld geheugen)      |
|                                    |
|  SQ: [ op | op | op |    |    ]    |  <-- cel schrijft
|                                    |
|  CQ: [ ok |    |    |    |    ]    |  <-- kernel schrijft
|                                    |
+------------------------------------+
  |
  |  Kernel leest de SQ, voert uit,
  |  zet het resultaat in de CQ
  v
Kernel
```

De **submission queue** (SQ) is waar de cel opdrachten neerzet:
"lees dit bestand", "stuur dit pakketje", "voer dit tegelprogramma uit". De
**completion queue** (CQ) is waar de kernel het resultaat terugzet: "klaar,
hier is het antwoord."

Dit is een **ringbuffer**: als je aan het einde komt, begin je weer vooraan.
Er is een kop (waar de consument leest) en een staart (waar de producent
schrijft). Zolang kop en staart niet botsen, kan alles door.

```text
Een ringbuffer (8 plekken)

schrijf-positie (staart)
         |
         v
[ _ | A | B | C | _ | _ | _ | _ ]
              ^
              |
    lees-positie (kop)
```

De cel hoeft niet te wachten tot de kernel klaar is. Ze zet meerdere
opdrachten in de ring, drukt een keer op de deurbel (`SYS_DOORBELL`), en gaat
verder met ander werk. Als het antwoord in de CQ verschijnt, wordt de strand
die erop wachtte gewekt. Dat is **batching** en **async** in een: meerdere
opdrachten tegelijk, zonder blokkeren.

De code voor het queue pair staat in `kernel/src/queue/mod.rs`. De on-wire
layout - hoe de bytes er in het geheugen uitzien - is gedefinieerd in `abi/`
zodat kernel en cel precies hetzelfde formaat gebruiken.

## De kracht van dit model

Waarom is dit zo'n krachtig idee? Omdat het **schaalt**. Dezelfde drie
eigenschappen (producent/consument, tegendruk, batching) werken op elke
schaal:

```text
Dezelfde drie regels, van klein naar groot:

Klein:    Spinlock          - een kern wil iets, wacht even, krijgt het
Middel:   NVMe-ring         - een driver stuurt acht opdrachten, wacht op acht antwoorden
Groot:    TCP-stroom        - twee computers sturen segmenten, met tegendruk via het venster
Enorm:    Datacenter        - duizenden servers verwerken taken uit een gedeelde wachtrij
```

Op elk niveau stel je dezelfde vragen:

1. Wie is de producent? Wie is de consument?
2. Wat gebeurt er als de rij vol raakt?
3. Kan ik meerdere items bundelen om efficienter te zijn?

Als je die drie vragen kunt beantwoorden voor een onderdeel dat je niet kent,
begrijp je al de helft van hoe het werkt.

## Tot slot: de scheidsrechter en zijn wachtrijen

In hoofdstuk 1 noemden we het OS de scheidsrechter. Nu weet je wat de
scheidsrechter echt doet: hij beheert wachtrijen. De scheduler heeft een rij
van programma's die willen draaien. De schijfdriver heeft een rij van
lees- en schrijfopdrachten. De netwerkstack heeft rijen van pakketjes in elke
richting. De GPU-driver heeft een rij van tegelprogramma's.

Het hele besturingssysteem is een netwerk van wachtrijen, verbonden door
producenten en consumenten, begrensd door tegendruk, versneld door bundeling.

```text
Het OS als wachtrijen

   +--------+    +----------+    +---------+
   | Timer  |--->| Scheduler|--->| CPU     |
   +--------+    | (rij van |    | (voert  |
                 | processen)|   | uit)    |
                 +----------+    +---------+

   +--------+    +----------+    +---------+
   | Prog.  |--->| Schijf-  |--->| NVMe    |
   | (lees!)|   | driver   |    | controller
   +--------+    | (SQ/CQ)  |    +---------+
                 +----------+

   +--------+    +----------+    +---------+
   | Prog.  |--->| TCP/IP   |--->| NIC     |
   | (stuur!)|   | stack    |    | (TX/RX) |
   +--------+    | (segment-|    +---------+
                 |  rijen)  |
                 +----------+

   +--------+    +----------+    +---------+
   | Prog.  |--->| Queue    |--->| Engine  |
   | (reken!)|   | pair     |    | (CPU/   |
   +--------+    | (SQ/CQ)  |    |  GPU)   |
                 +----------+    +---------+
```

Dat is het grote plaatje. En het is eenvoudiger dan je misschien had verwacht.

## Samenvatting

- **Alles in een computer is een wachtrij**: van de scheduler tot de
  netwerkkaart, van de schijf tot de GPU.
- Elke wachtrij heeft drie eigenschappen: **producent/consument**,
  **tegendruk** (backpressure) en **bundeling** (batching).
- Een HTTP-verzoek reist door minstens zes wachtrijen om van je browser bij
  de server te komen en terug.
- In rheo-os is het **queue pair** (een paar ringbuffers in gedeeld geheugen)
  de centrale manier waarop een programma met de kernel praat.
- Dezelfde drie vragen werken op elke schaal: van een spinlock tot een
  datacenter.
- Het besturingssysteem is een netwerk van verbonden wachtrijen, met de
  kernel als beheerder.

## Oefeningen

1. Kies een apparaat dat je dagelijks gebruikt (een printer, een lift, een
   kassa in de supermarkt). Beschrijf het als een wachtrij: wie is de
   producent, wie de consument, en wat is de tegendruk?
2. Volg het pad van een toetsaanslag door het systeem: van het indrukken van
   de toets tot het verschijnen van de letter op het scherm. Hoeveel
   wachtrijen passeer je?
3. Waarom is batching efficienter dan elk item apart verwerken? Geef een
   voorbeeld uit het dagelijks leven en een uit de computer.
4. Bekijk `kernel/src/queue/mod.rs` in rheo-os. Wat valt je op aan hoe de
   `QueueHeader` is opgebouwd? Welke velden herken je van de ringbuffer uit
   dit hoofdstuk?
5. Bedenk een situatie waarin *geen* tegendruk een groot probleem zou zijn.
   Wat gaat er mis?

---

Dit was het laatste hoofdstuk. Terug naar de [inhoudsopgave](README.md) en
de [woordenlijst](woordenlijst.md).
