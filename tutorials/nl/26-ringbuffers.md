# Hoofdstuk 26 - Ringbuffers: de ronde wachtrij

In het vorige hoofdstuk zag je dat wachtrijen overal zijn. Maar hoe bouw je er
eigenlijk een? Als je een gewone lijst gebruikt, moet je elke keer als je iets
van de voorkant haalt alles een plekje opschuiven. Dat is langzaam. Er is een
slimmer idee: de **ringbuffer**.

## Wat is een ringbuffer?

Een **ringbuffer** (ook wel: circulaire buffer) is een stuk geheugen met een
vast aantal plekken, en twee aanwijzers: een **kop** (head) en een **staart**
(tail). De schrijver schrijft op de plek waar de staart naar wijst en schuift
de staart op. De lezer leest op de plek waar de kop naar wijst en schuift de
kop op.

Het speciale: als de aanwijzer het einde van het stuk geheugen bereikt,
springt hij terug naar het begin. Alsof het geheugen een **ring** is in
plaats van een lijn.

```text
Stel: een ring met 8 plekken.

       kop                 staart
        v                    v
     +---+---+---+---+---+---+---+---+
     | . | A | B | C | D | . | . | . |
     +---+---+---+---+---+---+---+---+
       0   1   2   3   4   5   6   7

De lezer leest positie 1 (A), daarna 2 (B), ...
De schrijver schrijft op positie 5, daarna 6, ...

Als de staart voorbij positie 7 gaat, springt hij
terug naar 0. De ring draait rond.
```

Het mooie: er hoeft nooit data verschoven te worden. Je past alleen de twee
aanwijzers aan. Dat is heel snel - twee getallen bijwerken, klaar.

## Hoe weet je of de ring vol of leeg is?

- **Leeg:** kop en staart staan op dezelfde plek. Er is niets te lezen.
- **Vol:** de staart heeft de kop "ingehaald" (op een afstand van precies de
  ringgrootte). Er is geen ruimte meer om te schrijven.

De vuistregel: het aantal items in de ring is `staart - kop`. Als dat getal
nul is, is de ring leeg. Als het gelijk is aan de grootte van de ring, is hij
vol.

## Producent en consument zonder slot

Hier komt de echte magie. Als je precies een **schrijver** (producent) en een
**lezer** (consument) hebt - een zogenaamd **SPSC**-patroon (Single Producer,
Single Consumer) - dan kan de ringbuffer werken *zonder lock*.

Waarom? Omdat de schrijver alleen de staart aanpast, en de lezer alleen de
kop. Ze raken elkaars aanwijzer nooit aan. Zolang ze de juiste
geheugenbarriere gebruiken (zodat de ander hun schrijfactie echt ziet), werkt
het veilig.

```text
Producent                                Consument
   |                                        |
   |  schrijft data op positie [staart]     |
   |  verhoogt staart                       |
   |                                        |  leest data op positie [kop]
   |                                        |  verhoogt kop
   |                                        |
   v                                        v
   Raakt alleen "staart" aan               Raakt alleen "kop" aan
```

Dit is het patroon dat je terugziet in de event-ring van rheo-os
(`kernel/src/obs/ring.rs`). Elke CPU heeft zijn eigen ring. De CPU schrijft er
events in (producent) en een lezer - de kernel zelf, of een extern
hulpmiddel - leest ze eruit (consument). Geen slot nodig, want er is precies
een schrijver per ring.

## Macht van twee: waarom de grootte altijd 2, 4, 8, 16, ... is

Je hebt vast gezien dat ringbuffers bijna altijd een grootte hebben die een
**macht van twee** is: 8, 16, 32, 64, 1024, 2048. Dat is geen toeval.

Het probleem: als de aanwijzer het einde bereikt, moet hij terug naar het
begin. Normaal zou je dat doen met een rest-deling (`positie % grootte`).
Maar deling is langzaam voor een processor.

De truc: als de grootte een macht van twee is, kun je de deling vervangen
door een **bitmask** (een bitbewerking). Dat is een kwestie van een paar
bits wegknippen, en dat doet de processor in een enkele instructie.

```text
Grootte = 8 (binair: 1000)
Mask    = 7 (binair: 0111)

Positie 10 (binair: 1010)
10 & 7 = 2  (binair: 0010)

Positie 10 valt dus op plek 2 in de ring.
Dat is hetzelfde als 10 % 8 = 2, maar sneller.
```

In rheo-os zie je dit terug in `kernel/src/obs/ring.rs`, waar `RING_EVENTS`
2048 is (2^11), en de positie in de ring wordt berekend met een mask in
plaats van een deling.

## MPSC en MPMC: meerdere schrijvers of lezers

Het SPSC-patroon (een schrijver, een lezer) is het simpelste. Maar wat als je
**meerdere schrijvers** nodig hebt?

### MPSC: Multiple Producer, Single Consumer

Meerdere schrijvers, een lezer. Nu kunnen twee schrijvers tegelijk op
dezelfde plek proberen te schrijven. Je hebt een manier nodig om ze te
ordenen. Vaak gebruiken de schrijvers een **atomaire operatie** (zoals CAS
uit hoofdstuk 23) om de staart te claimen: "Ik neem plek 5, jij neemt plek 6."

### MPMC: Multiple Producer, Multiple Consumer

Meerdere schrijvers *en* meerdere lezers. Dit is het lastigste geval. Zowel
de kop als de staart moeten beschermd worden. Dit zie je in work-stealing
wachtrijen, waar elke processorcore zowel producent als consument kan zijn.

## Ringbuffers in de echte wereld

Het ring-patroon is zo nuttig dat het overal opduikt:

**virtio.** Het paravirtualisatie-protocol dat QEMU en andere hypervisors
gebruiken. Een virtuele schijf, netwerkkaart of GPU praat met de driver via
een gedeelde ring in het geheugen. De driver plaatst opdrachten in de ring,
het apparaat haalt ze eruit. De `virtqueue` in de virtio-specificatie is
precies een ringbuffer.

**io_uring.** De moderne Linux-manier om schijf- en netwerkoperaties te doen.
Een programma en de kernel delen twee ringen: een submission ring (opdrachten)
en een completion ring (antwoorden). Klinkt dat bekend? Het is dezelfde
structuur als de queue-pair in rheo-os.

**De queue-pair in rheo-os.** De communicatie tussen een cel en de kernel gaat
via een `QueuePair`: een submission queue (SQ) en een completion queue (CQ),
allebei ringbuffers in gedeeld geheugen. De definities staan in
`abi/src/lib.rs` (zoek naar `QueueHeader`, `RING_DEPTH`, `SqEntry`, `CqEntry`)
en de logica in `kernel/src/queue/mod.rs`.

```text
De queue-pair in rheo-os:

          cel (gebruikersprogramma)
              |           ^
   opdracht   |           |  antwoord
              v           |
         +----------+ +----------+
         |    SQ    | |    CQ    |    <- twee ringbuffers
         | (ring)   | | (ring)   |       in gedeeld geheugen
         +----------+ +----------+
              |           ^
              v           |
          kernel verwerkt
```

De cel schrijft een `SqEntry` in de SQ-ring, drukt op de deurbel
(`SYS_DOORBELL`), en de kernel leest het eruit, verwerkt het, en schrijft
een `CqEntry` in de CQ-ring. Geen kopie, geen vertaling - het is gedeeld
geheugen, en het ring-patroon maakt het snel.

**De event-ring.** Het observatievlak in rheo-os (`kernel/src/obs/ring.rs`)
gebruikt een ringbuffer per CPU om events vast te leggen. Elke CPU schrijft
alleen in zijn eigen ring (SPSC). De ring is 2048 events groot (64 KiB per
CPU), en bij een overloopt wordt het oudste event overschreven. Er is geen
slot nodig.

## Samenvatting

- Een **ringbuffer** is een stuk geheugen met een vaste grootte en twee
  aanwijzers (kop en staart) die ronddraaien. Er hoeft nooit data verschoven
  te worden.
- Bij **SPSC** (een schrijver, een lezer) werkt een ringbuffer zonder lock,
  omdat de twee kanten elkaars aanwijzer niet aanraken.
- Ringbuffers zijn bijna altijd een **macht van twee** groot, zodat de
  positieberekening een snelle bitmask-operatie kan zijn in plaats van een
  deling.
- **MPSC** (meerdere schrijvers) en **MPMC** (meerdere schrijvers en lezers)
  hebben extra bescherming nodig, vaak via atomaire operaties.
- Dit patroon zit overal: in **virtio** (virtuele apparaten), **io_uring**
  (Linux I/O), de **queue-pair** van rheo-os (`kernel/src/queue/mod.rs`), en
  de **event-ring** (`kernel/src/obs/ring.rs`).

## Oefeningen

1. Teken een ringbuffer met 4 plekken. Schrijf er drie items in (A, B, C) en
   lees er twee uit. Waar staan de kop en de staart na elke stap?
2. Waarom is een ringbuffer sneller dan een gewone lijst waar je items van
   de voorkant verwijdert?
3. Reken uit: als de ringgrootte 16 is en de staart op positie 19 staat,
   op welke plek in de ring valt dat? Gebruik de bitmask-truc.
4. Leg uit waarom een SPSC-ringbuffer zonder lock kan werken. Wat zou er
   misgaan als er twee schrijvers tegelijk waren zonder extra bescherming?
5. Bekijk `RING_DEPTH` in `abi/src/lib.rs`. Hoeveel opdrachten passen er
   tegelijk in de queue-pair ring van rheo-os? Is dat een macht van twee?
