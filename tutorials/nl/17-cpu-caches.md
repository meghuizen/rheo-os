# Hoofdstuk 17 - CPU-caches: waarom snelheid alles verandert

Dit hoofdstuk is een brughoofdstuk. Het gaat niet over code die je nu
schrijft, maar over een eigenschap van echte hardware die alles verandert
zodra je nadenkt over meerdere processoren en geheugen: de **cache**.

## Het snelheidsprobleem

In hoofdstuk 2 zagen we dat de processor rekent met **registers** (de zakjes)
en data ophaalt uit het **geheugen** (de rij postbusjes). Wat we toen niet
vertelden: het geheugen is *veel* langzamer dan de processor.

Stel je voor dat de CPU een kok is die razendsnel kan snijden. Maar elke keer
als hij een ingredient nodig heeft, moet hij naar een magazijn aan de andere
kant van de straat lopen. De kok staat het grootste deel van de dag te
*wachten*, niet te snijden.

In echte getallen:

- Een berekening met registers: ~1 nanoseconde (een miljardste seconde).
- Een getal uit het geheugen halen: ~100 nanoseconden.

Dat is een factor 100. De processor staat 99% van de tijd te wachten als hij
elke keer naar het geheugen moet. Dat is verspilling.

## De oplossing: caches

Een **cache** (spreek uit als "kesj") is een klein, snel stukje geheugen dat
vlak bij de processor zit. Het is de voorraadkast in de keuken: de kok hoeft
niet meer naar het magazijn als het ingredient al in de kast ligt.

Moderne processoren hebben meerdere lagen caches:

```text
+--------+
|  CPU   |
|  kern  |
+---+----+
    |  ~1 ns
+---+----+
| L1     |  32-64 KB, heel snel
+---+----+
    |  ~5 ns
+---+----+
| L2     |  256 KB - 1 MB, snel
+---+----+
    |  ~15 ns
+---+----+
| L3     |  4-32 MB, gedeeld tussen kernen
+---+----+
    |  ~100 ns
+---+----+
| RAM    |  4-64 GB, langzaam
+--------+
```

**L1** is het kleinst maar het snelst: 1-2 nanoseconden. **L2** is groter
maar iets langzamer. **L3** is nog groter en wordt vaak gedeeld tussen
meerdere processorkernen. En dan pas komt het echte RAM.

Vergelijk het met:
- **L1** = je broekzak (heel weinig past erin, maar je hebt het meteen).
- **L2** = je rugzak (meer ruimte, even zoeken).
- **L3** = je kluisje op school (gedeeld, je moet er even naartoe lopen).
- **RAM** = het magazijn aan de andere kant van de straat.

## Cachelijnen: alles gaat in blokjes

De cache haalt nooit een enkel getal uit het geheugen. Hij haalt altijd een
heel **blokje** van meestal **64 bytes** op. Zo'n blokje heet een
**cachelijn** (Engels: *cache line*).

Waarom? Omdat programma's bijna altijd de data *naast* het huidige getal ook
nodig hebben. Als je element 5 van een lijst leest, is de kans groot dat je
daarna element 6 wilt. Door een heel blokje in een keer op te halen, is
element 6 al in de cache. Dat heet **ruimtelijke lokaliteit** (spatial
locality).

Er is ook **tijdelijke lokaliteit** (temporal locality): data die je net
gebruikte, gebruik je waarschijnlijk snel weer. Daarom *blijft* data in de
cache totdat er een nieuwer blokje overheen wordt geschreven.

## Wat kost een cache miss?

Als de gevraagde data *niet* in de cache zit, heet dat een **cache miss**
(misser). De processor moet dan wachten tot het blokje uit het echte
geheugen is opgehaald. Een **cache hit** (treffer) is het tegenovergestelde:
de data lag al klaar.

Een eenvoudig rekenvoorbeeld:

Stel dat een programma 1.000.000 getallen uit een lijst leest.
- Bij 100% hits (alles in L1): 1.000.000 x 1 ns = **1 milliseconde**.
- Bij 10% misses (90% hits, 10% naar RAM): 900.000 x 1 + 100.000 x 100
  = 10.900.000 ns = **~11 milliseconden**.

Slechts 10% missers maakt het programma 11 keer langzamer. Daarom is het
schrijven van **cache-vriendelijke code** (data netjes naast elkaar, niet
her en der verspreid) een van de belangrijkste vaardigheden voor
prestatiegerichte programmeurs.

## Associativiteit: waar past een blokje?

Een cache is niet zomaar een rij vakjes waar je alles neerzet. De meeste
caches zijn **set-associatief**: elk geheugenadres kan maar in een beperkt
aantal plekken in de cache terecht. Hoe meer plekken (hogere associativiteit),
hoe minder vaak een nuttig blokje wordt verdrongen, maar hoe duurder de
hardware.

Dit hoef je niet te onthouden voor ons project, maar het verklaart waarom
bepaalde geheugenpatronen (data die steeds op dezelfde cacheplek terecht
komt) onverwacht langzaam kunnen zijn.

## Waarom caches cruciaal worden bij meerdere processoren

Tot nu toe hadden we het over een processor. Maar wat als er meerdere
processorkernen zijn die allemaal een eigen L1 en L2 cache hebben, en het
RAM delen?

```text
  Kern 0        Kern 1        Kern 2        Kern 3
  +----+        +----+        +----+        +----+
  | L1 |        | L1 |        | L1 |        | L1 |
  +----+        +----+        +----+        +----+
  | L2 |        | L2 |        | L2 |        | L2 |
  +----+        +----+        +----+        +----+
      \            |            |            /
       +--------+--+--+---------+----------+
                |  L3 (gedeeld)            |
                +-----------+--------------+
                            |
                      +-----+------+
                      |   RAM      |
                      +------------+
```

Als kern 0 een getal schrijft dat ook in de L1-cache van kern 1 zit, dan
heeft kern 1 een *verouderde kopie*. Dat mag niet: de hardware moet zorgen
dat alle caches het eens zijn. Dat proces heet **cache-coherentie**. De
kernen sturen berichtjes naar elkaar ("ik heb dit veranderd, gooi jouw kopie
weg"). Dat kost tijd.

Dit is het begin van het **NUMA-verhaal** (Non-Uniform Memory Access): niet
elk stukje geheugen is even snel bereikbaar voor elke kern. In rheo-os speelt
dit in `kernel/src/mm/frames.rs`, waar het frame-allocator-systeem bijhoudt
welke geheugenframes bij welke NUMA-node horen en probeert geheugen dicht
bij de juiste processorkern te plaatsen.

## False sharing: het stiekeme prestatieprobleem

Er is een verraderlijk probleem dat samenhangt met cachelijnen en meerdere
kernen. Het heet **false sharing** (vals delen).

Stel: twee kernen werken elk aan hun eigen variabele. De variabelen liggen
toevallig in dezelfde cachelijn van 64 bytes.

```text
Cachelijn (64 bytes):
+----------+----------+-----------------------------+
| var_A    | var_B    |  (rest van de 64 bytes)      |
+----------+----------+-----------------------------+
  ^                ^
  kern 0           kern 1
  schrijft         schrijft
```

Kern 0 verandert `var_A`. De hardware zegt nu tegen kern 1: "jouw kopie van
deze cachelijn is verouderd!" Kern 1 moet hem opnieuw ophalen. Daarna
verandert kern 1 `var_B`, en nu is kern 0 aan de beurt om opnieuw op te
halen. Ze slaan elkaars kopie steeds kapot, terwijl ze *niet eens dezelfde
variabele* gebruiken.

De oplossing is simpel maar belangrijk: zorg dat data die door verschillende
kernen wordt geschreven, op **verschillende cachelijnen** staat. In de
praktijk betekent dat: tussenruimte toevoegen (padding) of data per kern
apart houden. In rheo-os worden veel per-kern-structuren bewust gescheiden
gehouden (het `PerCpu<T>` type in `kernel/src/smp.rs`).

## Samenvatting

- Het geheugen is ~100x langzamer dan de processor. De **cache** overbrugt
  dat verschil.
- Caches zijn opgebouwd in lagen: **L1** (klein, snel), **L2** (groter),
  **L3** (gedeeld tussen kernen), en dan **RAM**.
- Data wordt opgehaald in blokjes van 64 bytes: **cachelijnen**.
- Een **cache miss** (data niet gevonden) kost veel tijd; een **cache hit**
  is bijna gratis.
- Bij meerdere kernen moeten caches het eens blijven: **cache-coherentie**.
- **False sharing** ontstaat als twee kernen per ongeluk dezelfde cachelijn
  beschrijven, ook al raken ze verschillende variabelen. Dat is te
  voorkomen door data per kern te scheiden.

## Oefeningen

1. Waarom is het geheugen zo veel langzamer dan de processor? Gebruik de
   vergelijking met de kok en het magazijn.
2. Reken uit: een programma leest 500.000 getallen. 95% is een cache hit
   (1 ns), 5% is een miss (100 ns). Hoe lang duurt het totaal? Hoeveel
   sneller zou het zijn bij 100% hits?
3. Wat is het verschil tussen **ruimtelijke lokaliteit** en **tijdelijke
   lokaliteit**? Geef van elk een voorbeeld.
4. Leg in je eigen woorden uit wat **false sharing** is en waarom het
   langzamer maakt in plaats van sneller.
5. Bekijk het diagram met de vier processorkernen. Waarom heeft elke kern
   een *eigen* L1-cache in plaats van een gedeelde? (Tip: denk aan de
   afstand.)

Door naar [hoofdstuk 18](18-geheugenbeheer.md): hoe het OS geheugen
uitdeelt.
