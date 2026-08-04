# Hoofdstuk 15 - Processen, threads en taken

Stel, je hebt drie programma's tegelijk open: een browser, een muziekspeler en
een teksteditor. In hoofdstuk 1 noemden we het OS de scheidsrechter die ze
laat afwisselen. Maar wat zijn die "programma's" eigenlijk vanuit het OS
gezien? In dit hoofdstuk leer je de drie lagen: **processen**, **threads** en
nog lichtere vormen zoals **taken** en **strands**.

## Een proces: een programma in zijn eigen speeltuin

Een **proces** is een draaiend programma met alles wat erbij hoort:

- Een eigen stuk geheugen (de **adresruimte**): het proces denkt dat het
  het geheugen helemaal voor zichzelf heeft. De paging-hardware uit
  hoofdstuk 18 maakt dat mogelijk.
- Een lijst van geopende bestanden (bestandsdescriptors).
- Rechten en permissies.
- Minstens een **uitvoerdraad** (execution thread): de plek waar de
  processor daadwerkelijk instructies uitvoert.

Vergelijk het met een huis. Het huis is het proces: eigen muren, eigen
deuren, eigen sleutels. In dat huis woont minstens een persoon die het
werk doet.

In rheo-os kun je de code van het native procesmodel bekijken in
`kernel/src/nproc.rs`. Daar regelt `SYS_SPAWN` het maken van een nieuw
proces (een nieuw "huis"), en `SYS_WAIT` laat het ouderproces wachten tot
het kindproces klaar is.

## Een thread: meerdere werkers in hetzelfde huis

Soms wil je *binnen* een proces meerdere dingen tegelijk doen. Denk aan een
browser: een draad tekent de pagina, een andere haalt data op van het
netwerk. Ze moeten allebei bij dezelfde pagina-inhoud (hetzelfde geheugen).
Een apart proces per taak zou betekenen dat ze hun geheugen niet makkelijk
kunnen delen.

Een **thread** (Nederlands: draad) is een extra uitvoerlijn *binnen* een
proces. Threads delen het geheugen en de bestanden van hun proces, maar elke
thread heeft zijn eigen:

- **Stack** (stapel): de plek waar lokale variabelen en retouradressen staan.
- **Registers**: de zakjes van de processor (uit hoofdstuk 2) worden per
  thread bijgehouden.
- **Program counter**: elke thread is op een andere plek in de code bezig.

```text
+----------- Proces -----------+
|                               |
|   [ Heap (gedeeld geheugen) ] |
|   [ Bestanden    (gedeeld)  ] |
|                               |
|  Thread 1    Thread 2         |
|  +-------+  +-------+        |
|  | Stack |  | Stack |        |
|  | Regs  |  | Regs  |        |
|  | PC    |  | PC    |        |
|  +-------+  +-------+        |
|                               |
+-------------------------------+
```

In het plaatje hierboven zie je: de heap en bestanden zijn gedeeld, maar
elke thread heeft zijn eigen stack en registers. Dat is het grote verschil
met processen: twee processen delen *niets* automatisch, twee threads
delen *bijna alles*.

In rheo-os vind je threads voor Linux-programma's in
`kernel/src/linux/thread.rs`. Daar is een thread een extra
**uitvoeringscontext** (een opgeslagen registerset plus een program counter)
binnen dezelfde cel. Threads delen een adresruimte en een kernelstack,
en wisselen af op syscall-grenzen.

## Waarom threads nuttig maar ook gevaarlijk zijn

Het voordeel is duidelijk: threads kunnen samenwerken aan dezelfde data
zonder trage kopieerstappen. Maar het nadeel is net zo duidelijk: als twee
threads tegelijk hetzelfde stukje geheugen veranderen, kan het misgaan. Dat
heet een **race condition** (wedstrijdprobleem). Het OS en de programmeur
moeten samen zorgen dat dit niet uit de hand loopt, met hulpmiddelen als
**mutexen** (sloten) en **semaphores** (tellers).

## Taken, fibers en strands: nog lichter

Soms zijn zelfs threads te zwaar. Een thread aanmaken kost tijd: het OS moet
registers klaarzetten, een stack klaarmaken, en de scheduler bijwerken.

Daarom bestaan er nog lichtere vormen van gelijktijdigheid:

- Een **taak** (task) of **coroutine** is een stukje werk dat *zelf* de
  processor teruggeeft als het even niets te doen heeft. Vergelijk het met
  een kookwekker: je zet water op, geeft de keuken vrij terwijl het water
  kookt, en gaat pas weer verder als de wekker afgaat. Dit heet
  **cooperatief**: de taak beslist zelf wanneer hij stopt.

- Een **fiber** of **green thread** is hetzelfde idee, maar dan beheerd door
  een bibliotheek in je programma, niet door het OS. Het OS ziet er maar een
  thread - de bibliotheek verdeelt die thread zelf over meerdere fibers.

- In rheo-os heten ze **strands** (draden). De code daarvoor staat in
  `runtime/`. Een strand is een Rust `Future` (een belofte van een
  resultaat). De **executor** (uitvoerder) in de runtime pakt strands op,
  voert ze uit tot ze "parkeren" op een wachtevenement (bijvoorbeeld: "ik
  wacht op antwoord van de wachtrij"), en pakt dan de volgende strand op.
  Dat is razendsnel: in metingen ~85 nanoseconden om een strand te starten,
  tegenover ~100.000 nanoseconden voor een OS-thread.

## Hoe de kernel het ziet vs. hoe de programmeur het ziet

Voor het OS (de kernel) bestaan er eigenlijk alleen **uitvoeringscontexten**:
een set registers, een program counter, en een adresruimte. Of jij dat een
"proces", "thread" of "taak" noemt, is een menselijk onderscheid.

```text
  Programmeur ziet:         Kernel ziet:

  Proces A                  Context 1 (adresruimte X)
    Thread 1                Context 2 (adresruimte X)
    Thread 2
                            Context 3 (adresruimte Y)
  Proces B
    Thread 1                Context 4 (adresruimte Y)
    Thread 2
```

De kernel moet bij elke wissel van context drie dingen weten:
1. Welke registers moet ik herstellen?
2. Welke adresruimte moet ik activeren?
3. Mag deze context nog draaien, of wacht hij ergens op?

Wisselen *binnen* een proces (thread-wissel) is goedkoop: de adresruimte
blijft hetzelfde, alleen registers worden verwisseld. Wisselen *tussen*
processen is duurder: ook de pagina-tabellen moeten worden gewisseld.

En strands/taken? Die bestaan helemaal niet voor de kernel. Alles wat de
kernel ziet is een thread die af en toe een syscall doet. De bibliotheek
binnen het programma regelt de rest.

## Samenvatting

- Een **proces** is een draaiend programma met een eigen adresruimte,
  bestanden en rechten. Processen delen niets automatisch.
- Een **thread** is een uitvoerdraad *binnen* een proces. Threads delen
  geheugen, maar hebben elk een eigen stack en registers.
- **Taken**, **fibers** en **strands** zijn nog lichtere vormen van
  gelijktijdigheid, vaak beheerd door een bibliotheek, niet door de kernel.
- De kernel ziet uiteindelijk alleen **uitvoeringscontexten**: registers,
  een program counter en een adresruimte.
- Wisselen binnen een proces is goedkoper dan wisselen tussen processen.

## Oefeningen

1. Wat is het belangrijkste verschil tussen een proces en een thread?
2. Waarom is het gevaarlijk dat threads geheugen delen? Bedenk een voorbeeld
   van wat er mis kan gaan als twee threads dezelfde variabele veranderen.
3. Leg in je eigen woorden uit wat een strand (of taak/fiber) is, en waarom
   het sneller is dan een echte thread.
4. Een webserver ontvangt 1000 verzoeken per seconde. Zou je voor elk verzoek
   een apart proces, een thread of een taak/strand gebruiken? Waarom?
5. Bekijk `kernel/src/linux/thread.rs` in rheo-os. Wat is het eerste dat het
   commentaar bovenaan je vertelt over hoe threads hier werken?

Door naar [hoofdstuk 16](16-scheduling.md): wie krijgt de processor?
