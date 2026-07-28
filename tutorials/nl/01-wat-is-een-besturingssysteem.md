# Hoofdstuk 1 - Wat is een besturingssysteem?

## Het probleem dat een OS oplost

Een computer kan maar één ding heel goed: heel snel simpele bewerkingen doen.
Optellen, vergelijken, iets uit het geheugen halen, iets terugzetten. Meer niet.

Maar jij wilt tegelijk muziek luisteren, een spelletje spelen en een berichtje
typen. Dat zijn drie programma's die alle drie de processor willen gebruiken. Ze
willen ook alle drie geheugen. En ze willen alle drie bij het scherm en het
toetsenbord.

Als je die programma's zomaar hun gang laat gaan, gaat het mis:

- Ze schrijven door elkaars geheugen heen en crashen.
- Eén programma pakt de processor en laat de andere nooit meer aan de beurt.
- Ze grijpen alle drie tegelijk naar het scherm.

Een **besturingssysteem** is de scheidsrechter die dit netjes regelt.

## De taken van een besturingssysteem

Een OS heeft een paar hoofdtaken. We noemen ze nu even kort; later in het boek
bouw je ze zelf.

1. **Programma's laten afwisselen.** De processor kan maar één ding tegelijk.
   Het OS laat programma A heel even werken, dan B, dan C, heel snel achter
   elkaar. Het lijkt daardoor of ze tegelijk draaien. Dit heet **scheduling**
   (Nederlands: inplannen).

2. **Geheugen verdelen.** Het OS geeft elk programma zijn eigen stukje geheugen
   en zorgt dat programma A niet in het geheugen van programma B kan komen. Dat
   is **geheugenbescherming**.

3. **Met de hardware praten.** Het scherm, het toetsenbord, de schijf, het
   netwerk - een programma mag daar niet zomaar zelf bij. Het vraagt het aan het
   OS. Het OS heeft speciale stukjes code, **drivers**, die met elk apparaat
   kunnen praten.

4. **Diensten aanbieden.** "Open dit bestand", "geef me het huidige tijdstip",
   "start een nieuw programma". Zulke verzoeken heten **systeemaanroepen**
   (Engels: *system calls*, meestal afgekort tot *syscalls*).

## Kernel en gebruikersruimte

Een OS bestaat uit twee werelden:

- De **kernel** (Nederlands: kern). Dit is het hart van het OS. De kernel mag
  *alles*: bij alle hardware, bij al het geheugen. Daarom moet de kernel heel
  betrouwbaar zijn. Eén fout hier kan de hele computer laten crashen.

- De **gebruikersruimte** (Engels: *user space*). Hier draaien gewone
  programma's: je browser, je spelletje. Die mogen *niet* alles. Als ze iets van
  de hardware nodig hebben, vragen ze het netjes aan de kernel met een syscall.

Waarom die scheiding? Voor veiligheid. Als je browser een fout heeft, mag dat
niet je hele computer platleggen. De kernel houdt de browser binnen zijn eigen
speeltuin.

De processor helpt hierbij. Hij kan in twee standen draaien:

- **Kernel-stand** (ook wel "supervisor" of "ring 0"): alles mag.
- **Gebruikers-stand** (ook wel "user mode" of "ring 3"): beperkt.

Een programma in gebruikers-stand dat iets verbodens probeert, wordt door de
processor tegengehouden. De processor springt dan automatisch naar de kernel, en
die beslist wat er gebeurt (meestal: het programma netjes afbreken).

## Een plaatje in woorden

Denk aan een school:

- De **kernel** is de conciërge met alle sleutels. Hij mag overal komen.
- De **programma's** zijn de leerlingen. Ze mogen in hun eigen lokaal, maar niet
  zomaar het magazijn in.
- Wil een leerling iets uit het magazijn? Dan vraagt hij het aan de conciërge.
  Dat vragen is een **syscall**.
- De conciërge zorgt ook dat de lokalen om de beurt de gymzaal (de processor)
  mogen gebruiken. Dat is **scheduling**.

## Wat bouwen wij in dit boek?

We bouwen het begin van een kernel. In het begin is er nog geen gebruikersruimte
en zijn er nog geen andere programma's. Er is alleen onze eigen code, die als
kernel draait. Stap voor stap voegen we meer toe.

De allereerste stap is: zorgen dat onze code überhaupt gaat draaien als de
computer aangaat. Daarvoor hebben we een **bootloader** nodig. Wat dat is en hoe
het werkt, zie je in [hoofdstuk 5](05-hoe-een-computer-opstart.md).

## Samenvatting

- Een OS is de scheidsrechter tussen programma's die allemaal de processor, het
  geheugen en de hardware willen.
- Hoofdtaken: programma's inplannen (scheduling), geheugen verdelen en
  beschermen, met hardware praten (drivers), diensten aanbieden (syscalls).
- De **kernel** mag alles; **gebruikersprogramma's** mogen beperkt en vragen de
  rest aan de kernel.
- De processor heeft een kernel-stand en een gebruikers-stand om dit af te
  dwingen.

## Oefening

1. Noem drie taken van een besturingssysteem in je eigen woorden.
2. Waarom is het gevaarlijk als een gewoon programma alles zou mogen? Bedenk een
   voorbeeld van wat er dan mis kan gaan.
3. Leg het verschil tussen "kernel" en "gebruikersruimte" uit met een eigen
   vergelijking (niet die van de school).

Door naar [hoofdstuk 2](02-hoe-werkt-een-cpu.md), waar we in de processor kijken.
