# Hoofdstuk 22 - Vergrendelen in de kernel

In het vorige hoofdstuk leerde je dat meerdere cores tegelijk draaien en
dat gedeelde data problemen geeft. De klassieke oplossing: een
**vergrendeling** (Engels: *lock*). Maar er zijn meerdere soorten, elk met
eigen voor- en nadelen. En vergrendelingen kunnen ook zelf problemen
veroorzaken. In dit hoofdstuk leer je de belangrijkste soorten en de
valkuilen.

## Spinlock: wachten door te draaien

De eenvoudigste vergrendeling is de **spinlock** (draaislot). Het idee:

1. Een vlaggetje zegt of het slot open of dicht is.
2. Wil je erbij? Probeer het vlaggetje atomair op "dicht" te zetten (met
   compare-and-swap uit hoofdstuk 20).
3. Was het al dicht? Probeer het opnieuw. En opnieuw. En opnieuw. Je
   "draait" (spint) in een lusje tot het slot opengaat.
4. Ben je klaar? Zet het vlaggetje terug op "open".

```text
  Spinlock: verkrijgen en loslaten

  Core 0                          Core 1
  ------                          ------
  probeer slot --> lukt!          probeer slot --> bezet
  [doe je werk]                   probeer slot --> bezet
  [doe je werk]                   probeer slot --> bezet
  laat slot los                   probeer slot --> lukt!
                                  [doe je werk]
                                  laat slot los
```

**Voordeel**: heel simpel en snel als het slot bijna nooit bezet is.
Er is geen hulp van het OS nodig.

**Nadeel**: als het slot lang bezet is, verspilt de wachtende core zijn
hele tijd aan het "draaien" in het lusje. Hij doet niets nuttigs. Dat heet
**busy waiting** (bezig wachten).

Spinlocks zijn ideaal in de kernel voor korte stukjes code die snel klaar
zijn, zoals het bijwerken van een teller of een lijstje. In rheo-os staat
de `SpinLock<T>` in `kernel/src/smp.rs`. Hij is gebouwd op een
`AtomicBool`: een atomaire boolean die "open" of "dicht" is.

## Mutex: wachten door te slapen

Een **mutex** (van *mutual exclusion*, wederzijdse uitsluiting) werkt
hetzelfde als een spinlock, maar met een belangrijk verschil: als het slot
bezet is, gaat de wachtende thread *slapen* in plaats van draaien. Het OS
zet hem aan de kant en geeft de processor aan iemand anders. Als het slot
vrijkomt, wordt de slapende thread wakker gemaakt.

Vergelijk het zo: bij een spinlock sta je voor een gesloten deur en blijf
je op de deur bonken. Bij een mutex ga je op een stoel zitten en zegt de
portier je wanneer de deur opengaat.

**Voordeel**: de processor wordt niet verspild aan wachten. Goed voor
vergrendelingen die lang bezet kunnen zijn.

**Nadeel**: het slapen en wakker worden kost extra tijd (de context switch
uit eerdere hoofdstukken). Als het slot heel kort bezet is, is een spinlock
sneller.

**Vuistregel**: in de kernel gebruik je spinlocks (je kunt daar soms niet
slapen, bijvoorbeeld in een interrupt-handler). In gebruikersruimte gebruik
je mutexen.

## Semafoor: een teller als slot

Een **semafoor** (Engels: *semaphore*) is een vergrendeling met een teller
in plaats van een vlaggetje. De teller begint op een getal N, en:

- **Wacht** (P of "down"): als de teller groter is dan 0, verlaag hem en
  ga door. Is de teller 0? Dan wacht je.
- **Signaleer** (V of "up"): verhoog de teller. Als er iemand wacht, maak
  die wakker.

Een semafoor met N=1 werkt precies als een mutex. Maar met N=3 kun je
drie cores tegelijk toelaten, bijvoorbeeld drie threads die tegelijk een
beperkt aantal bestanden mogen openen.

Stel je het voor als een parkeergarage met N plekken: als er plek is, rij
je erin. Is het vol? Dan wacht je buiten tot iemand wegrijdt.

## RCU: lezers hoeven nooit te wachten

**RCU** staat voor *Read-Copy-Update*. Het is een slimme techniek uit
Linux voor de situatie waarin je heel veel lezers hebt en bijna nooit een
schrijver. Het idee in drie stappen:

1. **Lezen** gaat altijd door, zonder vergrendeling. Een lezer pakt gewoon
   een verwijzing naar de huidige versie van de data.
2. **Schrijven** maakt een **kopie** van de data, past de kopie aan, en
   verwisselt de verwijzing atomair. Nieuwe lezers zien de nieuwe versie.
3. **Opruimen**: de oude versie wordt pas vrijgegeven als alle lezers die
   hem nog gebruikten, klaar zijn. Dat moment heet een **grace period**
   (gratieperiode).

```text
  RCU: lezen zonder slot, schrijven via een kopie

  Lezers (altijd vrij):           Schrijver:
  ---------------------           ----------
  lezer A leest versie 1          1. kopieert versie 1 --> versie 2
  lezer B leest versie 1          2. past versie 2 aan
                                  3. verwisselt pointer: nu wijst alles
                                     naar versie 2
  lezer C leest versie 2          4. wacht tot A en B klaar zijn
  (A en B lezen nog versie 1)     5. ruimt versie 1 op
```

**Voordeel**: lezen is razend snel - geen vergrendeling, geen wachten.
**Nadeel**: schrijven is duurder (kopieren, wachten op de grace period).

RCU is perfect voor datastructuren die heel vaak gelezen worden en zelden
veranderen, zoals routeringstabellen of configuratie.

## Seqlock: snel lezen, achteraf controleren

Een **seqlock** (sequence lock, volgordelot) is een andere slimme truc.
Het werkt zo:

1. Er is een teller die bij elke schrijfactie twee keer wordt verhoogd:
   een keer voor het schrijven begint (naar een oneven getal) en een keer
   als het klaar is (naar een even getal).
2. Een lezer leest de teller, leest de data, en leest de teller opnieuw.
3. Als de teller veranderd is of oneven was, weet de lezer dat er een
   schrijver tussenin zat. Dan probeert hij het opnieuw.

```text
  Seqlock: lezen met controle achteraf

  Schrijver:                 Lezer:
  ----------                 ------
                             leest teller = 4 (even, goed)
  teller = 5 (begint)        leest data
  schrijft data              leest teller = 5 (veranderd!)
  teller = 6 (klaar)         --> opnieuw proberen
                             leest teller = 6 (even, goed)
                             leest data
                             leest teller = 6 (gelijk, klaar!)
```

**Voordeel**: heel snel voor lezers als er bijna nooit een schrijver is.
Geen vergrendeling nodig voor de lezer.
**Nadeel**: de lezer moet soms opnieuw, en de data moet "veilig om
verkeerd te lezen" zijn (geen verwijzingen die halverwege kapot zijn).

Seqlocks zijn ideaal voor dingen als de systeemklok: die wordt heel vaak
gelezen (elke `clock_gettime`) en maar af en toe bijgewerkt (elke
timer-interrupt).

## Wat kan er misgaan: deadlocks

Een **deadlock** (doodslot) ontstaat als twee cores op elkaar wachten en
geen van beiden verder kan:

```text
  Core 0: pakt slot A, wil slot B --> wacht op core 1
  Core 1: pakt slot B, wil slot A --> wacht op core 0

  Geen van beiden komt verder. De computer "hangt".
```

Dit is als twee mensen in een smal gangetje die allebei wachten tot de
ander aan de kant gaat.

De oplossing: zorg dat vergrendelingen altijd in dezelfde **volgorde**
worden gepakt. Als iedereen altijd eerst A pakt en dan B, kan de situatie
hierboven niet ontstaan.

## Wat kan er misgaan: prioriteitsinversie

**Prioriteitsinversie** (priority inversion) is een ander probleem.
Stel je drie taken voor met lage (L), gemiddelde (M) en hoge (H)
prioriteit:

1. Taak L pakt een slot.
2. Taak H wil hetzelfde slot --> moet wachten op L.
3. Taak M (die het slot niet nodig heeft) loopt ondertussen, want M heeft
   hogere prioriteit dan L.
4. L kan niet verder omdat M de processor bezet houdt.
5. H kan niet verder omdat L het slot houdt.

Resultaat: de taak met de *hoogste* prioriteit wacht het langst, omdat de
taak met de *laagste* prioriteit wordt tegengehouden door de taak in het
midden. Dat is precies het omgekeerde van wat je wilt.

De klassieke oplossing heet **priority inheritance** (prioriteitsovererving):
als een taak met hoge prioriteit op een slot wacht, krijgt de slothouder
tijdelijk de hoge prioriteit zodat hij snel kan afmaken.

## Vergrendeling in rheo-os

In rheo-os vind je vergrendelingen op twee plekken:

- **`SpinLock<T>`** in `kernel/src/smp.rs`: de universele kernelvergrendeling.
  Altijd gecompileerd, ook op een enkele core (daar is hij altijd vrij,
  dus kost hij niets). Gebruikt voor de frame-allocator, de toelatingsbeslissing
  van de scheduler, en de PMEM-allocator.

- **`plock`** in `kernel/src/linux/mod.rs`: de vergrendeling over de hele
  Linux-persoonlijkheid. Dit is een "grote sluis"-aanpak: alle Linux-
  systeemaanroepen gaan door dezelfde vergrendeling heen. Het is
  **recursief per CPU** - een systeemaanroep die vanuit zichzelf weer bij
  het slot uitkomt (via een pagina-fout), loopt niet vast. En op een
  enkele core wordt het slot helemaal overgeslagen, zodat het geen
  vertraging geeft.

De `SpinLock` is de eenvoudige variant: hij draait in een lusje op een
`AtomicBool`. Dat werkt hier goed omdat de kernel zijn kritieke secties
kort houdt - een paar instructies, en dan is het slot weer vrij.

## Samenvatting

- Een **spinlock** draait in een lusje tot het slot vrijkomt. Simpel en
  snel voor korte secties. Verspilt de processor bij lang wachten.
- Een **mutex** laat de wachtende thread slapen. Beter voor lange secties.
- Een **semafoor** is een teller die N threads tegelijk toelaat.
- **RCU** laat lezers nooit wachten door te kopieren en later op te
  ruimen.
- **Seqlocks** laten lezers zonder slot lezen en achteraf controleren.
- **Deadlocks** ontstaan als twee cores op elkaars slot wachten. Oplossing:
  pak sloten altijd in dezelfde volgorde.
- **Prioriteitsinversie**: een hoge-prioriteit taak wacht op een
  lage-prioriteit taak die wordt tegengehouden. Oplossing:
  prioriteitsovererving.

## Oefeningen

1. Wanneer kies je een spinlock en wanneer een mutex? Geef voor elk een
   situatie.
2. Beschrijf in je eigen woorden hoe RCU werkt, met de drie stappen.
3. Twee cores pakken sloten in de volgorde A-dan-B en B-dan-A. Teken wat
   er kan misgaan.
4. Een lezer leest een seqlock-teller en krijgt het getal 7 (oneven).
   Wat moet de lezer doen?
5. Zoek in `kernel/src/smp.rs` hoe `SpinLock::lock()` werkt. Welke
   atomaire operatie gebruikt hij om het slot te pakken?

Terug naar de [inhoudsopgave](README.md).
