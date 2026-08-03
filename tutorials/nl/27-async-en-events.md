# Hoofdstuk 27 - Asynchroon en event-gestuurd: wachten zonder stilstaan

Stel je een ober voor in een restaurant. Als hij een bestelling naar de keuken
brengt en dan bij het tafeltje gaat staan wachten tot het eten klaar is, kan hij
maar een klant tegelijk bedienen. Dat is wat een computer doet als hij
**blokkerend** wacht op de schijf of het netwerk: de processor doet niets nuttigs
terwijl de data onderweg is. In dit hoofdstuk leer je hoe een besturingssysteem -
en de programma's die erop draaien - slim kunnen wachten, zodat een ober honderd
tafeltjes tegelijk kan bedienen.

## Het probleem: wachten kost processortijd

De schijf is ruwweg honderdduizend keer langzamer dan de processor. Het netwerk
kan miljoenen keer langzamer zijn. Als je programma een bestand leest met een
gewone **blokkerende** aanroep, staat de processor stil tot de bytes er zijn.

Dat is prima als je programma maar een ding tegelijk doet. Maar een webserver
bedient misschien duizend bezoekers tegelijk. Als elke bezoeker zijn eigen
**thread** (draadje) krijgt en die thread blokkeert op het netwerk, heb je
duizend threads die bijna allemaal staan te wachten. Elke thread kost geheugen
(vaak 1-8 MB aan **stack**) en het wisselen tussen duizenden threads kost ook
processortijd. Dat schaalt niet.

## Twee smaken: blokkerend versus niet-blokkerend

Er zijn twee manieren om I/O (invoer/uitvoer) te doen:

1. **Blokkerend**: je vraagt om data en je programma staat stil tot die er is.
   Simpel, maar verspillend als je veel moet wachten.

2. **Niet-blokkerend**: je vraagt om data en je krijgt meteen antwoord: "er is
   nu niets, probeer later nog eens." Jij kunt dan eerst iets anders doen.

Niet-blokkerend klinkt beter, maar "steeds opnieuw proberen" (dat heet
**pollen**) is ook verspilling. De truc is: laat het besturingssysteem je
*vertellen* wanneer er iets klaar is. Dat is **event-gestuurd** werken.

## Drie oplossingen in de echte wereld

### select/poll/epoll (Linux)

De oudste aanpak is **select** (en zijn opvolger **poll**): je geeft het OS een
lijst van bronnen (netwerkaansluitingen, bestanden) en zegt "laat me weten als
er op *een* ervan iets te lezen valt." Het OS blokkeert je programma op die ene
plek, niet op duizend plekken tegelijk.

**epoll** is de snellere versie op Linux. In plaats van de hele lijst elke keer
opnieuw te geven, registreer je je bronnen een keer en het OS houdt ze bij.

### io_uring (Linux, modern)

**io_uring** draait het idee om. In plaats van te vragen "is er iets klaar?"
*lever je werk in* op een **indienrij** (submission queue) en het OS meldt de
resultaten op een **voltooiingsrij** (completion queue). Dat lijkt op een
bakkerij met nummertjes: je doet je bestelling, gaat zitten, en je nummer wordt
omgeroepen als het brood klaar is.

```text
  Jouw programma                    Kernel
  +--------------------+            +--------------------+
  | "lees bestand X"   |--indienwachtrij-->|              |
  | "schrijf naar Y"   |            | verwerkt in de     |
  |                    |            | achtergrond        |
  |                    |<--voltooiingswachtrij--|         |
  | "X is gelezen!"    |            |                    |
  | "Y is geschreven!" |            |                    |
  +--------------------+            +--------------------+
```

Het voordeel: je programma en de kernel hoeven geen informatie heen en weer te
kopieren bij elk verzoek. De twee rijen liggen in **gedeeld geheugen** en de
processor hoeft niet eens naar de kernel te springen voor elk verzoek.

### IOCP (Windows)

Op Windows heet hetzelfde idee **I/O Completion Ports** (IOCP). Je dient werk in
en wacht op een voltooiingsmelding. Het idee - indienen, later ophalen - is
hetzelfde als io_uring, alleen de details en namen verschillen.

## De event-loop: een ober met een notitieboekje

Het patroon dat al deze methoden gebruikt heet de **event-loop** (gebeurtenis-
lus). Het werkt zo:

1. Registreer alles waar je op wilt wachten (netwerkaansluitingen, timers,
   bestanden).
2. Vraag het OS: "welke van mijn bronnen is klaar?"
3. Behandel alles wat klaar is.
4. Ga terug naar stap 2.

```text
  +---> Wacht op events (epoll / io_uring / IOCP)
  |          |
  |          v
  |     Welke bronnen zijn klaar?
  |          |
  |     +----+----+----+
  |     |    |    |    |
  |     v    v    v    v
  |    client  client  timer  bestand
  |    A klaar B klaar afgelopen gelezen
  |     |    |    |    |
  |     +----+----+----+
  |          |
  |          v
  |     Verwerk ze allemaal
  |          |
  +----------+
```

Die ene lus kan makkelijk tienduizend verbindingen bedienen, want het OS doet
het echte wachten. De lus doet alleen werk als er *echt* iets te doen is.

## Hoe programmeertalen dit gebruiken

### Rust: async/await

In Rust schrijf je `async fn lees() -> Bytes` en wacht je met `.await`. Onder de
motorkap bouwt de compiler je functie om in een **toestandsmachine** (state
machine). De **executor** (uitvoerder) draait al die toestandsmachines in een
event-loop. De populairste executor heet **tokio** en gebruikt epoll of io_uring
onder Linux. Je code *lijkt* gewoon op elkaar volgend, maar is eigenlijk
event-gestuurd.

### Node.js

Node.js draait je JavaScript in een enkele thread met een event-loop (via
**libuv**). Als je `fs.readFile(...)` aanroept, gaat het lezen op de
achtergrond en je callback wordt aangeroepen als het klaar is. Daarom is Node
heel goed in veel verbindingen tegelijk, maar niet in zwaar rekenwerk (dat
blokkeert de enige thread).

### Python: asyncio

Python heeft **asyncio**: ook een event-loop, ook `async`/`await`. Omdat Python
de beruchte **GIL** (Global Interpreter Lock) heeft, kan er maar een thread
tegelijk Python draaien. asyncio werkt daar slim omheen door I/O-wachtmomenten
te gebruiken om naar de volgende taak te springen - precies het event-loop-idee.

## rheo-os: strands en de reactor

rheo-os heeft zijn eigen versie van dit verhaal. In `runtime/` vind je de
**strand executor** (`runtime/src/strand.rs`). Een **strand** is een lichtgewicht
taak (een Rust `Future`). Als een strand moet wachten, "parkeert" hij zich op
een **token** - een nummertje. Ondertussen draait de executor andere strands.

In `librheo/src/rt.rs` zit de **reactor**: de event-loop van een cel. De reactor
stuurt werk naar de kernel via een **queue pair** (een paar rijen in gedeeld
geheugen, net als io_uring) en haalt voltooiingen op. Als een voltooiing
terugkomt, bevat die het token van de strand die erop wachtte, en die strand
wordt wakker gemaakt.

```text
  Cel (gebruikersprogramma)          Kernel
  +---------------------------+      +-------------------+
  | strand A: "lees bestand"  |      |                   |
  |   -> submit OP_READ (token=7)    |                   |
  |   -> parkeer op token 7   |      | voert OP_READ uit |
  |                           |      |                   |
  | strand B: draait door!    |      |                   |
  |                           |      |                   |
  | reactor: ring doorbell    |      |                   |
  |   <- voltooiing token=7   |<-----|                   |
  |   -> maak strand A wakker |      |                   |
  +---------------------------+      +-------------------+
```

Het verschil met een gewone event-loop: de strands zijn *stackless* (ze hebben
geen eigen stapel geheugen), waardoor ze extreem licht zijn. rheo-os meet het:
een strand wisselen kost ongeveer 12 nanoseconden - ruwweg 1.500 keer sneller
dan een OS-thread.

## Samenvatting

- **Blokkerend** wachten verspilt de processor. Met duizend verbindingen heb je
  duizend slapende threads.
- **Event-gestuurd** werken lost dat op: registreer waar je op wacht, laat het OS
  je wakker maken als er iets klaar is.
- Linux biedt hiervoor **epoll** (wachten op klaar) en **io_uring** (indienen en
  ophalen via gedeelde rijen).
- De **event-loop** is het patroon: een lus die wacht, verwerkt en weer wacht.
- Rust (async/await), Node.js en Python (asyncio) bouwen hier allemaal bovenop.
- rheo-os heeft **strands** (lichtgewicht taken) en een **reactor** (de
  event-loop van een cel), verbonden via een **queue pair** - het io_uring-idee,
  maar dan als kern van het OS.

## Oefeningen

1. Leg in je eigen woorden uit waarom een webserver met duizend blokkerende
   threads slecht schaalt. Gebruik de ober-vergelijking.
2. Wat is het verschil tussen **pollen** en **event-gestuurd** wachten? Waarom is
   het tweede beter?
3. io_uring en rheo-os gebruiken allebei een paar gedeelde rijen (submission en
   completion queue). Teken ze en leg uit wat er in elke richting gaat.
4. Bekijk `runtime/src/strand.rs` in de rheo-os code. Zoek de plek waar een
   strand zich "parkeert" op een token. Wat gebeurt er met de executor als alle
   strands geparkeerd zijn?
5. Waarom is Node.js goed in veel verbindingen maar slecht in zwaar rekenwerk?

Door naar [hoofdstuk 28](28-zero-copy.md): gegevens verplaatsen zonder kopieren.
