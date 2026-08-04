# Hoofdstuk 16 - Scheduling: wie krijgt de processor?

In hoofdstuk 15 zagen we dat een computer tientallen of honderden taken
tegelijk wil draaien. Maar een processorkern kan maar een ding tegelijk.
Het OS moet dus steeds kiezen: **wie is nu aan de beurt?** Dat kiezen heet
**scheduling** (inplannen). In dit hoofdstuk leer je hoe die keuze gemaakt
wordt, waarom het lastiger is dan je denkt, en wat er misgaat als je het
verkeerd doet.

## Het simpelste idee: om de beurt

Stel, je hebt drie taken: A, B en C. De makkelijkste aanpak is
**round-robin**: iedereen krijgt een even lang stukje tijd (een
**tijdschijf**, Engels: *time slice*), en daarna gaat de volgende.

```text
Tijd --->

  A |####|    |####|    |####|
  B |    |####|    |####|    |####
  C               |         |    |####

     ^    ^    ^    ^    ^    ^
     |    |    |    |    |    |
   wissel van taak (de scheduler kiest)
```

Klinkt eerlijk, toch? Iedereen komt even vaak aan de beurt. Maar wat als
taak A iets heel dringends doet (een alarm!) en taak C alleen maar rustig
aan het rekenen is? Met round-robin maakt dat niet uit: iedereen wacht
even lang.

## Prioriteiten: sommige taken zijn belangrijker

Om dat op te lossen geven veel besturingssystemen taken een
**prioriteit**: een getal dat zegt hoe belangrijk de taak is. Een
hogere prioriteit betekent: je komt eerder aan de beurt.

Maar prioriteiten brengen een nieuw probleem. Stel dat taak A (hoge
prioriteit) altijd de processor wil. Dan komt taak C (lage prioriteit)
*nooit* aan de beurt. Dat heet **uithongering** (Engels: *starvation*).
De scheidsrechter is niet eerlijk meer: hij laat de sterke speler altijd
winnen.

## Wat kan er misgaan? Twee klassieke problemen

### Uithongering (starvation)

Zoals net beschreven: een taak met lage prioriteit krijgt nooit de kans
om te draaien, omdat taken met hogere prioriteit altijd klaarstaan.

### Prioriteitsinversie (priority inversion)

Dit is nog gemener. Stel:

1. Taak L (lage prioriteit) heeft een slot (mutex) in handen.
2. Taak H (hoge prioriteit) wil datzelfde slot en moet dus wachten.
3. Taak M (midden prioriteit) draait ondertussen vrolijk door.

Het resultaat: H (de belangrijkste taak) wacht op L, maar L komt niet aan
de beurt omdat M er steeds tussen zit. De *middelste* blokkeert de
*hoogste* - via de laagste. Dit is echt gebeurd: de Mars Pathfinder-rover
liep hier in 1997 vast.

De oplossing heet **priority inheritance**: als H wacht op het slot van L,
dan krijgt L tijdelijk de prioriteit van H, zodat L snel klaar kan zijn.

## Moderne strategieen

Door de jaren heen zijn slimmere aanpakken bedacht. Hier de belangrijkste:

### CFS (Completely Fair Scheduler)

Dit was jarenlang de standaard in Linux. Het idee: houd bij hoeveel
**virtuele tijd** elke taak heeft gehad. De taak met de *minste* virtuele
tijd gaat eerst. Zo wordt iedereen vanzelf eerlijk behandeld, zonder vaste
tijdschijven.

### EEVDF (Earliest Eligible Virtual Deadline First)

De opvolger van CFS in nieuwere Linux-versies. Elke taak krijgt een
**virtuele deadline**: wanneer hij uiterlijk aan de beurt moet zijn. De
taak met de vroegste deadline die *ook* aan zijn beurt is (eligible) gaat
eerst. Het slimme: een taak die een klein stukje werk vraagt, krijgt een
eerdere deadline en reageert sneller - zonder dat iemand een prioriteit
hoeft in te stellen.

### EDF (Earliest Deadline First)

Vergelijkbaar, maar dan met **echte deadlines**. Gebruikt voor taken die
echt op tijd klaar moeten zijn: een geluidsprogramma dat elke 5
milliseconden een stukje geluid moet afleveren. Als het te laat is, hoor
je een klik. Dit heet **hard-realtime scheduling**.

### BORE (Burst-Oriented Response Enhancement)

BORE kijkt naar het **gedrag** van een taak. Een taak die kort rekent en
dan weer gaat wachten (interactief, zoals typen) krijgt voorrang. Een taak
die lang achter elkaar rekent (een berekening, een video omzetten) wordt
iets minder snel bediend. Niemand hoeft iets in te stellen: het OS
*meet* wat de taak doet.

In rheo-os wordt BORE gecombineerd met EEVDF. De code staat in
`kernel/src/sched/bore.rs` (de burst-score) en `kernel/src/sched/vcore.rs`
(de wachtrij op basis van virtuele deadlines). De burst-score is de
**bitlengte** van de rekentijd - een hele snelle berekening (een
`leading_zeros`-instructie), want de scheduler mag zelf niet langzaam zijn.

## Cooperatief vs. preemptief

Er zijn twee manieren om taken te laten afwisselen:

### Cooperatief (de taak geeft de processor terug)

De taak beslist *zelf* wanneer hij stopt. Vergelijk het met een vergadering
waar iedereen beleefd het woord doorgeeft. Voordeel: simpel, geen
verrassingen. Nadeel: als een taak het woord niet doorgeeft, staat iedereen
stil.

In rheo-os begon de scheduler cooperatief: een cel (zo heet een proces hier)
geeft de processor terug bij een syscall (bijvoorbeeld `SYS_YIELD`). De code
daarvoor staat in `kernel/src/nproc.rs`.

### Preemptief (de timer pakt de processor af)

Het OS zet een **hardwaretimer**. Als de timer afgaat (een interrupt!), pakt
het OS de processor af, ongeacht wat de taak aan het doen is. Vergelijk het
met een voorzitter die na twee minuten de microfoon uitzet.

In rheo-os staat de preemptiecode in `kernel/src/sched/preempt.rs`. De
timer-arbiter (`kernel/src/ktimer.rs`) beheert een hardwaretimer die per
processorkern werkt. Als de timer afgaat, zet de interruptafhandeling een
vlaggetje, en bij terugkeer naar de taak beslist het OS of de processorkern
naar een andere taak gaat.

De meeste echte besturingssystemen zijn preemptief. Cooperatief is simpeler
en werkt goed als je alle code zelf schrijft, maar zodra er een programma is
dat niet meewerkt (of een bug heeft waardoor het in een lus blijft hangen),
heb je preemptie nodig.

## De scheduler in actie

Hier een vereenvoudigd beeld van wat er bij elke wissel gebeurt:

```text
  Taak A draait
       |
  [timer-interrupt!]
       |
  OS slaat registers van A op
       |
  Scheduler kiest: wie nu?
       |
  OS herstelt registers van B
       |
  Taak B draait
```

Dat "registers opslaan en herstellen" heet een **context switch**
(contextwisseling). Op rheo-os duurt dat ongeveer 150 instructies voor een
strand (de lichte variant) en meer voor een volledige proceswisseling (want
dan moeten ook de pagina-tabellen gewisseld worden).

## Samenvatting

- **Scheduling** is kiezen wie de processor krijgt.
- **Round-robin** is eerlijk maar dom: iedereen wacht even lang.
- **Prioriteiten** helpen, maar brengen risico's: **uithongering** en
  **prioriteitsinversie**.
- Moderne aanpakken: **CFS** (virtuele tijd), **EEVDF** (virtuele
  deadlines), **EDF** (harde deadlines), **BORE** (gedrag meten).
- **Cooperatief**: de taak geeft de processor terug.
  **Preemptief**: de timer pakt hem af.
- Een **context switch** is het opslaan en herstellen van registers bij
  een wissel van taak.

## Oefeningen

1. Leg uit wat **uithongering** is en geef een voorbeeld met drie taken.
2. Waarom is preemptieve scheduling veiliger dan cooperatieve? Bedenk
   een situatie waarin cooperatief misgaat.
3. Een muziekprogramma moet elke 5 ms geluid afleveren. Welk type
   scheduling past daar het best bij (round-robin, prioriteit, EDF)?
   Waarom?
4. Bekijk `kernel/src/sched/bore.rs` in rheo-os. Het commentaar legt uit
   dat de burst-score wordt berekend met `leading_zeros`. Waarom is het
   belangrijk dat die berekening snel is?
5. Wat is prioriteitsinversie? Waarom is het zo verraderlijk dat zelfs
   een ruimtesonde er last van had?

Door naar [hoofdstuk 17](17-cpu-caches.md): waarom snelheid alles
verandert.
