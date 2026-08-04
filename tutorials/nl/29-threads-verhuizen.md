# Hoofdstuk 29 - Threads verhuizen tussen processoren

Moderne computers hebben meerdere processor-kernen (cores). Dat is alsof je
keuken meerdere koks heeft: ze kunnen tegelijk aan verschillende gerechten
werken. Maar wie bepaalt welke kok welk gerecht maakt? En wat gebeurt er als
een gerecht halverwege naar een andere kok verhuist? In dit hoofdstuk leer je hoe
het besturingssysteem threads (draadjes) over kernen verdeelt, en waarom dat soms
sneller en soms juist langzamer is.

## CPU-affiniteit: een thread vastpinnen

Standaard mag het OS een thread op elke beschikbare kern draaien. Maar soms wil
je een thread **vastpinnen** aan een bepaalde kern. Dat heet **CPU-affiniteit**
(CPU affinity).

Waarom zou je dat willen?

- **Warme cache**: als een thread steeds op dezelfde kern draait, zitten zijn
  data nog in de **cache** van die kern - het snelle geheugen vlak bij de
  processor. Verhuizen naar een andere kern betekent dat de cache daar "koud"
  is: de data moet opnieuw uit het trage hoofdgeheugen gehaald worden.

- **Voorspelbaarheid**: voor heel tijdkritisch werk (geluid, video, robots) wil
  je geen verrassingen. Een vastgepinde thread hoeft nooit te verhuizen.

Maar er is een keerzijde:

- **Onevenwicht**: als je kern 0 vastpint met drie zware threads en kern 1 niets
  te doen heeft, staat de helft van je processor stil. Het OS kan het werk niet
  herverdelen.

Pinnen is dus een gereedschap, geen wondermiddel. Gebruik het alleen als je weet
dat het helpt.

## NUMA: niet al het geheugen is even snel

In een gewone laptop is al het geheugen even ver van de processor. Maar in
grotere machines (servers, werkstations) is dat niet zo. Daar heeft elke groep
kernen zijn **eigen** geheugenbank, en het geheugen van de *andere* groep is
verder weg en langzamer.

Dit heet **NUMA**: *Non-Uniform Memory Access* (niet-gelijkmatig
geheugentoegang). Denk aan twee keukens met elk een eigen voorraadkast. Elke kok
kan bij beide kasten, maar naar de kast in de andere keuken lopen kost extra
tijd.

```text
  +------------------+          +------------------+
  |   Node 0         |          |   Node 1         |
  |                  |          |                  |
  |  Kern 0  Kern 1  |          |  Kern 2  Kern 3  |
  |       |          |          |       |          |
  |  [Geheugen 0]    |---bus----|  [Geheugen 1]    |
  |  (snel voor 0,1) |          |  (snel voor 2,3) |
  +------------------+          +------------------+
         |                              |
         +--- geheugen van de andere ---+
              node is langzamer
```

Een slim OS houdt hier rekening mee: het probeert het geheugen van een thread
op dezelfde **node** te plaatsen als de kern waar die thread draait. Dat heet
**NUMA-bewuste plaatsing**.

## Migratie: wat gebeurt er als een thread verhuist?

Soms besluit het OS een thread naar een andere kern te verplaatsen. Dat heet
**migratie**. Het OS doet dit om de werklast eerlijk te verdelen: als kern 0
overbelast is en kern 3 niets te doen heeft, verhuist er een thread.

Maar migratie heeft een prijs:

1. **Koude cache**: de nieuwe kern heeft nog niets in zijn cache van deze thread.
   De eerste instructies zijn langzamer omdat alles opnieuw opgehaald moet
   worden. Dit heet een **cold start** (koude start).

2. **TLB-leeggooi**: de **TLB** (Translation Lookaside Buffer) is een klein
   cache-geheugen voor adresvertalingen (van het virtuele adres dat je programma
   ziet naar het fysieke adres in het echte geheugen). Bij een verhuizing is de
   TLB van de nieuwe kern leeg voor deze thread, en elke adresvertaling moet
   opnieuw opgezocht worden.

3. **NUMA-afstand**: als de thread verhuist naar een kern op een *andere* node,
   moet hij nu zijn geheugen over de langzame bus bereiken. Dat kan een flinke
   vertraging geven.

```text
  Kern 0 (druk)              Kern 3 (vrij)
  +-------------+            +-------------+
  | Thread X    |            |             |
  | cache: warm |            | cache: koud |
  | TLB: gevuld |  --migratie-->  | TLB: leeg   |
  +-------------+            +-------------+
                               Thread X moet
                               alles opnieuw
                               ophalen
```

De scheduler moet dus een afweging maken: is het voordeel van een betere
verdeling groter dan de kosten van de verhuizing?

## Work stealing: een vrije kern pakt werk

Een slimme manier om werk te verdelen heet **work stealing** (werk stelen). Het
idee is simpel:

- Elke kern heeft een eigen **werklijst** (work queue) met taken.
- Als een kern klaar is met al zijn taken, kijkt hij bij een *andere* kern en
  pakt daar een taak vandaan.

Dat is als een kok die klaar is met al zijn bestellingen en bij de stapel van
een drukke collega een bordje wegpakt. Eerlijk en efficient: niemand staat stil
zolang er ergens werk is.

Work stealing vermijdt het probleem van vooraf verdelen: je hoeft niet van
tevoren te weten hoeveel werk elke kern krijgt. De vrije kernen verdelen het
vanzelf.

## Heterogene kernen: grote en kleine koks

Veel moderne processoren hebben niet alleen *meerdere* kernen, maar ook
*verschillende soorten*. ARM noemt dit **big.LITTLE**: grote, snelle kernen en
kleine, zuinige kernen. Intel noemt het **P-cores** (Performance, snel) en
**E-cores** (Efficiency, zuinig).

Denk aan een restaurant met meesterkoks en leerling-koks. De meesterkoks zijn
sneller maar kosten meer. Simpele gerechten kun je prima door de leerling laten
doen.

De scheduler moet nu een extra beslissing nemen: *welk soort kern* krijgt deze
taak?

- **Zwaar rekenwerk** (een video renderen, een database-query) gaat naar een
  grote/snelle kern.
- **Licht werk** (wachten op een toetsaanslag, af en toe een klein berichtje
  verwerken) gaat naar een kleine/zuinige kern. Dat bespaart stroom.

Hoe weet de scheduler wat "zwaar" en "licht" is? Hij kan het *meten*: een taak
die lang achtereen de processor bezighoudt zonder te pauzeren is "zwaar"; een
taak die steeds even iets doet en dan weer wacht is "licht". Dat is precies wat
de **BORE**-score doet: de *burst time* (hoe lang een taak achtereen draaide)
bijhouden en daar de prioriteit op aanpassen.

## rheo-os: BORE, NUMA en heterogene kernen

rheo-os implementeert deze ideeen:

### BORE in `kernel/src/sched/bore.rs`

De **BORE-score** (Burst-Oriented Response Enhancement) meet hoe lang een taak
achtereen de processor gebruikte. Korte bursts (interactief werk) krijgen een
hogere prioriteit; lange bursts (rekenwerk) een lagere. Het mooie: in rheo-os is
elke pauze een expliciete systeemaanroep, dus de meting is een *observatie*
in plaats van een schatting.

### NUMA-plaatsing in `kernel/src/mm/`

In `kernel/src/mm/frames.rs` houdt de allocator bij welke geheugenblokken op
welke NUMA-node liggen. Als een cel geheugen vraagt, probeert de allocator een
blok te pakken van de node waar die cel thuishoort. Lukt dat niet (de node is
vol), dan wijkt hij uit naar een andere node en telt die uitwijking
(`numa_fallbacks`). Zo weet je precies hoe vaak het geheugen "ver weg" terecht
is gekomen.

### Heterogene kernen

In `kernel/src/sched/bore.rs` en de bijbehorende plaatsingslogica in
`kernel/src/smp.rs` wordt elke kern geclassificeerd: Performance, Efficiency,
of Unknown. Zwaar rekenwerk gaat naar een snelle kern; licht werk naar een
zuinige. Als QEMU geen verschillende soorten kernen kan nabootsen (en dat kan
het niet), worden de regels getest met een kunstmatige tweedeling en geverifieerd
met een host-model-checker in `verify/hetero/`.

### Work stealing

Als een kern klaar is met alle taken die hij heeft gekregen en er staat nog werk
op een andere kern dat nog niet is begonnen, mag hij dat overnemen. De `smp`-test
in rheo-os bewijst dit: bij 8 taken op 4 kernen neemt een vrije kern een extra
taak over van een drukke collega, en dat wordt geteld en gecontroleerd.

## Samenvatting

- **CPU-affiniteit** pint een thread aan een kern: goed voor warme caches, slecht
  voor verdeling.
- **NUMA** betekent dat niet al het geheugen even snel is. Het OS plaatst
  geheugen liefst dicht bij de kern die het gebruikt.
- **Migratie** verplaatst een thread naar een andere kern. Dat kost een koude
  cache en een lege TLB, maar verdeelt het werk beter.
- **Work stealing** laat een vrije kern werk pakken bij een drukke collega.
  Efficient zonder vooraf te plannen.
- **Heterogene kernen** (P-core/E-core, big.LITTLE) vragen de scheduler om te
  beslissen welk *soort* kern een taak krijgt: snel of zuinig.
- rheo-os meet de burst-duur met BORE, plaatst geheugen NUMA-bewust, en laat
  kernen werk stelen bij drukke collega's.

## Oefeningen

1. Wanneer is het vastpinnen van een thread aan een kern een goed idee, en
   wanneer niet? Geef van elk een voorbeeld.
2. Teken een NUMA-systeem met twee nodes. Een thread op node 0 leest geheugen
   van node 1. Leg uit waarom dat langzamer is.
3. Een thread verhuist van kern 0 naar kern 3. Noem drie dingen die langzamer
   worden vlak na de verhuizing.
4. Leg **work stealing** uit met de koks-in-een-keuken-vergelijking. Waarom is
   het beter dan vooraf verdelen?
5. Een processor heeft twee snelle P-cores en twee zuinige E-cores. Hoe zou de
   scheduler een videospel (zwaar) en een chat-app (licht) verdelen?

Door naar [hoofdstuk 30](30-talen-en-runtimes.md): hoe programmeertalen hun eigen
lichtgewicht taken bovenop het OS bouwen.
