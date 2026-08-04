# Hoofdstuk 30 - Hoe talen het doen: .NET, Java, Go en Python

Een OS-thread is een krachtig ding: een eigen stapel geheugen, een plek in de
scheduler, registerbewaring bij elke wissel. Maar al dat krachtige kost ook:
een thread aanmaken kost tienduizenden nanoseconden, wisselen enkele duizenden,
en de stapel neemt 1-8 MB geheugen in. Als je een miljoen gelijktijdige taken
wilt (denk aan een chatserver), heb je een probleem.

Daarom bouwen programmeertalen *lichtere* dingen bovenop OS-threads. In dit
hoofdstuk bekijken we hoe vijf talen dat doen, en hoe hun oplossingen
samenwerken - of botsen - met de scheduler van het besturingssysteem.

## Het basisidee: M:N-afbeelding

De kern van bijna elke lichtgewicht-oplossing is **M:N-afbeelding**: je hebt M
gebruikerstaken en N OS-threads, waarbij M veel groter is dan N. De N
OS-threads zijn de "echte" draadjes die het OS kent en inplant; de M taken zijn
lichtgewicht dingen die de taal *zelf* verdeelt over die N threads.

```text
  M gebruikerstaken (licht, goedkoop)
  +-+ +-+ +-+ +-+ +-+ +-+ +-+ +-+
  |1| |2| |3| |4| |5| |6| |7| |8|    <- de taal plant in
  +-+ +-+ +-+ +-+ +-+ +-+ +-+ +-+
   \   |   /     \  |  /    \   /
    \  |  /       \ | /      \ /
  +------+       +------+   +------+
  | OS   |       | OS   |   | OS   |  <- het OS plant in
  |thread|       |thread|   |thread|
  | A    |       | B    |   | C    |
  +------+       +------+   +------+
     |              |          |
  +------+       +------+   +------+
  |Kern 0|       |Kern 1|   |Kern 2|  <- de hardware
  +------+       +------+   +------+
```

Denk aan een school met drie lokalen (de kernen). De conciergerie (het OS) deelt
drie klassen (OS-threads) in over de lokalen. Maar binnen elke klas zitten
tientallen leerlingen (de gebruikerstaken) die zelf afwisselen wie het woord
heeft. De conciergerie hoeft niets te weten van die leerlingen - alleen van de
drie klassen.

## Go: goroutines en het G-M-P-model

Go is ontworpen met dit idee als kern. Een **goroutine** is een lichtgewicht
taak die je aanmaakt met het woordje `go`. Een goroutine kost ongeveer 4 KB
geheugen (tegenover 1-8 MB voor een OS-thread) en er kunnen er makkelijk
honderdduizenden tegelijk bestaan.

Onder de motorkap heeft Go drie bouwstenen:

- **G** (Goroutine): een gebruikerstaak. Licht en goedkoop.
- **M** (Machine): een OS-thread. Zwaar, maar nodig om echt op een kern te
  draaien.
- **P** (Processor): een logische processor. Elke P heeft een werklijst met
  goroutines en is gekoppeld aan een M.

```text
  +-----+  +-----+  +-----+  +-----+  +-----+
  | G   |  | G   |  | G   |  | G   |  | G   |   goroutines
  +-----+  +-----+  +-----+  +-----+  +-----+
     \       /          |        \       /
      \     /           |         \     /
    +-------+       +-------+    +-------+
    | P 0   |       | P 1   |    | P 2   |       logische processors
    |werklijst      |werklijst   |werklijst      (elk met eigen lijst)
    +-------+       +-------+    +-------+
       |               |            |
    +-------+       +-------+    +-------+
    | M 0   |       | M 1   |    | M 2   |       OS-threads
    +-------+       +-------+    +-------+
       |               |            |
    [Kern 0]        [Kern 1]     [Kern 2]         hardware
```

Als een goroutine blokkeert (bijvoorbeeld op een netwerkaanroep), haalt Go de
P los van de M en koppelt die P aan een andere M. De geblokkeerde M wacht
netjes op zijn netwerkaanroep, en de P gaat door met andere goroutines. Zo hoeft
geen kern stil te staan.

Go gebruikt ook **work stealing**: als een P geen goroutines meer heeft, steelt
hij er een van een andere P. Dat is hetzelfde idee als in hoofdstuk 29.

## Java (JVM) en .NET (CLR): virtuele machines

Java en .NET (C#) draaien op een **virtuele machine** (de JVM en de CLR). Dat is
een stuk software dat een standaard-processor nabootst en extra diensten levert:

- Een **garbage collector** (vuilnisman) die ongebruikt geheugen automatisch
  opruimt.
- Een **JIT-compiler** (Just-In-Time) die je programma tijdens het draaien
  vertaalt naar machinecode van de echte processor.
- Een **thread pool** (dradengroep): een vaste set OS-threads die klaarstaan om
  taken uit te voeren.

### .NET: Task en async/await

In C# maak je een lichtgewicht taak aan met `Task.Run(...)`. Die taak wordt niet
meteen een eigen OS-thread. In plaats daarvan komt hij op de **thread pool**: een
groep van een handvol OS-threads die taken oppakken zodra ze vrij zijn. Dat is
M:N-afbeelding: duizenden Tasks op tientallen OS-threads.

C# heeft ook `async`/`await`, dat werkt als de event-loop uit hoofdstuk 27:
als een taak wacht op I/O, geeft hij de OS-thread vrij en pakt de thread pool
een andere taak. Alles draait op dezelfde handvol threads.

### Java: ThreadPool en virtual threads

Java had altijd "zware" threads: elke Java-thread was een echte OS-thread. Dat
werkte prima tot je duizenden tegelijk wilde.

Sinds Java 21 (Project Loom) heeft Java **virtual threads** (virtuele draden):
lichtgewicht taken die, net als goroutines, door de runtime over een handvol
OS-threads verdeeld worden. Je maakt ze aan met `Thread.ofVirtual()` en ze
kosten bijna niets. Het is Java's antwoord op Go's goroutines: M:N-afbeelding,
met de JVM als tussenpersoon.

## Python: de GIL en asyncio

Python is een bijzonder geval. De standaard Python-implementatie (CPython) heeft
een **GIL**: de *Global Interpreter Lock* (globaal interpreterslot). Die GIL
zorgt ervoor dat er maar **een** thread tegelijk Python-code mag uitvoeren, ook
al heb je meerdere kernen.

Waarom? Omdat de interne boekhoudingen van de Python-interpreter (vooral de
garbage collector) niet ontworpen zijn voor gelijktijdige toegang. De GIL maakt
het simpel en veilig, maar je kunt geen zwaar rekenwerk versnellen met meerdere
threads.

```text
  Python met 4 threads en de GIL:

  Kern 0:  [Thread 1 draait Python]
  Kern 1:  [Thread 2 wacht op GIL...] }
  Kern 2:  [Thread 3 wacht op GIL...] } slechts 1 tegelijk!
  Kern 3:  [Thread 4 wacht op GIL...] }
```

Python's oplossing voor I/O-taken is **asyncio**: een event-loop (zie hoofdstuk
27) die taken laat afwisselen op momenten dat ze wachten op het netwerk of de
schijf. De GIL is dan geen probleem, want er is maar een thread die afwisselend
aan al die taken werkt.

Voor echt parallel rekenwerk gebruikt Python **multiprocessing**: aparte
processen (geen threads) die elk hun eigen GIL hebben. Dat werkt, maar processen
zijn zwaarder dan threads en delen geen geheugen.

## Rust: geen runtime, wel keuze

Rust is anders dan al het bovenstaande. Rust heeft **geen ingebouwde runtime**.
Standaard krijg je gewone OS-threads, net zo zwaar als die van C. Maar Rust
heeft `async`/`await` in de taal zelf, en je kiest zelf je executor.

De populairste executor is **tokio**: die beheert een thread pool en een
event-loop, vergelijkbaar met Go's model maar zonder garbage collector. Een
Rust `async fn` wordt door de compiler omgebouwd in een toestandsmachine -
precies zo licht als een goroutine, maar zonder de runtime-overhead van een
virtuele machine.

Omdat er geen standaard-runtime is, kies je als programmeur precies wat je nodig
hebt: een volle tokio-runtime, een minimale executor, of gewoon OS-threads. Die
vrijheid is Rust's kracht en tegelijk zijn drempel.

## Twee schedulers in een: samenwerken of botsen

Als een taal zijn eigen scheduler heeft (Go, Java, .NET, Python-asyncio), heb
je twee schedulers die tegelijk draaien:

1. De **runtime-scheduler** van de taal: verdeelt gebruikerstaken over
   OS-threads.
2. De **OS-scheduler**: verdeelt OS-threads over kernen.

Die twee weten niets van elkaar. Dat werkt meestal goed, maar soms botsen ze:
het OS verhuist een thread naar een andere kern terwijl de runtime dat niet
weet (koude cache), of de runtime maakt een nieuwe thread aan terwijl het OS
alle kernen bezet heeft (wachtrij). Python's GIL maakt het nog erger: het OS
verdeelt vier threads over vier kernen, maar de GIL laat er maar een tegelijk
echt draaien.

## Vergelijkingstabel

```text
  Taal     | Eenheid        | Model | Stapel  | Wisseltijd  | Opmerkingen
  ---------+----------------+-------+---------+-------------+---------------------
  C / OS   | OS-thread      | 1:1   | 1-8 MB  | ~1-5 us     | zwaar, betrouwbaar
  Go       | goroutine      | M:N   | ~4 KB   | ~0.3 us     | work stealing, GMP
  Java 21+ | virtual thread | M:N   | ~klein  | ~0.5 us     | Project Loom
  .NET/C#  | Task           | M:N   | geen*   | ~0.5 us     | async/await + pool
  Python   | asyncio taak   | M:1** | geen*   | ~0.5 us     | GIL, 1 thread echt
  Rust     | async taak     | M:N   | geen*   | ~0.01 us    | geen runtime; tokio
  rheo-os  | strand         | M:N   | geen*   | ~0.01 us    | queue-pair reactor
  ---------+----------------+-------+---------+-------------+---------------------
  * stackless: de toestand zit in een klein struct, niet op een eigen stapel
  ** Python's GIL maakt het in de praktijk M taken op 1 echte draaiende thread
```

De tijden zijn schattingen; de verhouding is het punt, niet de precieze waarden.

## rheo-os: strands als lichtgewicht taken

rheo-os volgt het Rust-pad: in `runtime/` staat de strand-executor. Een
**strand** is een `Future` (een toestandsmachine die de Rust-compiler maakt van
`async` code). Strands zijn **stackless**: ze hebben geen eigen stapel
geheugen, waardoor ze extreem klein zijn.

De gemeten getallen: een strand aanmaken kost ~85 nanoseconden, wisselen ~12
nanoseconden. Ter vergelijking: een OS-thread aanmaken kost ~100.000
nanoseconden - ruwweg **1.200 keer langzamer**. Go's goroutines zitten daar
tussenin.

Het verschil zit in wat er *niet* gebeurt: geen OS-trap, geen kernelcode, geen
register-bewaring van de hele processor, geen adresruimte-wissel. Een
strand-wissel is gewoon een Rust-functie die een struct opslaat en een andere
oppakt.

## Samenvatting

- OS-threads zijn krachtig maar zwaar: veel geheugen, dure wisseling.
- Programmeertalen bouwen **lichtgewicht taken** bovenop: goroutines (Go),
  virtual threads (Java), Tasks (C#), asyncio-taken (Python), async-taken
  (Rust).
- Het kernidee is **M:N-afbeelding**: veel gebruikerstaken op weinig OS-threads.
- Go gebruikt het G-M-P-model met work stealing; Java en .NET gebruiken een
  thread pool met een virtuele machine; Python heeft de GIL die echte
  parallelliteit verhindert.
- Rust heeft geen vaste runtime: je kiest zelf je executor (zoals tokio).
- Twee schedulers (de runtime en het OS) draaien tegelijk en weten niets van
  elkaar - dat werkt meestal, maar kan soms botsen.
- rheo-os's strands zijn extreem licht (~12 ns wissel) omdat ze stackless zijn
  en geen OS-trap nodig hebben.

## Oefeningen

1. Leg in je eigen woorden uit wat **M:N-afbeelding** is. Gebruik de
   school-vergelijking (klassen en leerlingen).
2. Go's scheduler haalt een P los van een geblokkeerde M. Waarom is dat slimmer
   dan de hele P laten wachten?
3. Waarom maakt Python's GIL het zinloos om vier CPU-intensieve threads te
   draaien op vier kernen? Welke oplossing heeft Python daarvoor?
4. Een Rust `async fn` wordt omgebouwd in een toestandsmachine. Wat is het
   voordeel van "stackless" (geen eigen stapel) ten opzichte van goroutines
   (die wel een eigen stapel hebben)?
5. Bekijk de vergelijkingstabel. Waarom is de wisseltijd van een strand of een
   Rust async-taak zoveel korter dan die van een OS-thread? Noem minstens twee
   dingen die niet hoeven te gebeuren.

Terug naar de [inhoudsopgave](README.md).
