# Meals: cooking from building blocks

A Drive app for planning, prepping and cooking with **components**
(building blocks you make once and combine many ways) instead of fixed
recipes. Written 2026-11-20.

## The idea, from the video

Ethan Chlebowski, *How to Eat Chicken Breast Every Day (without going
crazy)*, 18 Nov 2026 (https://youtu.be/1m9ReeKwJ4I), teaches three levels,
each a framework that multiplies combinations:

1. **Technique.** One way to cook a base well: flatten the breast to an
   even thickness, salt at 1–1.5% of its weight (2 g for 200 g), spice
   generously (check blends for salt), sear in a hot pan (~425 °F surface)
   that stays hot. The cooked base then goes into **form factors**: a wrap,
   a sandwich, a salad, a rice bowl. Four spice blends × four form factors
   = 16 meals from one technique.
2. **Deferred flavour.** Batch-cook the base plain (whole, strips or
   cubes; 1% salt), store it unflavoured, and flavour it when eating, by
   one of three **methods**: bloom spices in oil in the pan and toss the
   cold base through; toss the warm base with a dressing or vinaigrette
   (chimichurri); glaze it with a sauce in the pan (teriyaki, made ahead
   by weight and kept in a squeeze bottle). 3 methods × 4 variations × 4
   form factors = 48 meals from one prep.
3. **The plate.** Four slots: a **protein** (seared, roasted, grilled, or
   breaded and fried), a **starch** (mash, rice, bread, tortillas, orzo),
   a **vegetable** (a salad, roast vegetables, a slaw, pickles), and a
   **sauce** (a pan sauce, tomato, curry, a mayo-based dressing). Four of
   each = 256 plates. Classic dishes (chicken parm, piccata, katsu curry)
   are instances of it. Components (a Caesar dressing, a sprout slaw, a
   parmesan orzo) are prepped ahead and reused across meals.

Throughout, amounts are by weight, and each meal gets a calorie, protein
and cost breakdown.

**What the app takes from it:** components are the unit, not recipes;
flavour is often a separate component applied late; meals are slots
filled from what is prepped; the number of possible meals is the point,
and the app should show it.

## The landscape (2026)

- **Component-first apps are rare and small:** PrepFlow (iOS: "cook a few
  useful bases once and turn them into different dinners", tracks
  prepared bases); Meal Prep for Batch Cooking UK (batch-cook
  "Foundations"); Mealprapp (a German "Meal Prep Baukasten", tied to a
  course). None models methods, slots and the fridge together.
- **Recipe-first planners:** Paprika, Plan to Eat (manual, shared), Mealime,
  Samsung Food (free, grocery retailers), Prepear; **AI planners:** Ollie
  (iOS, families), Eat This Much (macros); **macro-first:** MealPrepPro.
  **Build-your-own bowl** ordering (The Meal Preps, Qdoba, Luna Grill)
  shows the slot-and-macros UI people already know.
- **Self-hosted, open source:** Mealie, Tandoor (recipe managers). From
  memory; not checked for this note.
- **Recipe text:** Cooklang, a plain-text recipe markup with a Rust parser
  (`cooklang` crate, 0.18.x: scaling, unit conversion, aisles).

## The model (Drive document types, namespace `meals.`)

| Type | What it is |
|---|---|
| `Ingredient` | Something bought: nutrition per 100 g, cost per unit, aisle, how it keeps. A shared catalog per household, seeded from public data (USDA FoodData Central). |
| `Technique` | A way of cooking a kind of base, with parameters: salt % by weight, pan temperature, time by cut (whole, strips, cubes), and steps. |
| `Component` | A building block you make and store: plain salted chicken cubes, teriyaki sauce, chimichurri, Caesar dressing, sprout slaw, parmesan orzo. Has **roles** (protein, starch, vegetable, sauce, flavour, topping), a recipe (Cooklang text, so it scales and converts), a yield in grams, how it is stored and how long it keeps. Nutrition and cost per gram are computed from its ingredients. |
| `Flavour` | A component applied late, with a **method**: bloom in pan, toss with dressing, glaze with sauce. Usable on any base whose role allows it. |
| `Template` | A form factor or framework with **slots**: the plate (protein, starch, vegetable, sauce), a rice bowl (base, protein, flavour, vegetable, topping), a wrap, a sandwich, a salad. Each slot accepts roles and has a default amount. |
| `Meal` | A template with its slots filled and amounts set. Its calories, protein and cost are computed. Can be saved, rated, repeated. |
| `Plan` | Meals placed on days and times for a household. |
| `PrepSession` | A batch: which components to make and how much, worked backwards from the plan; an ordered, timed checklist. |
| `Fridge` | What is prepped and on hand: each component's amount left and when it was made (so when it expires), and raw stock. |
| `ShoppingList` | What the prep needs, minus what is on hand, by aisle. |

## What the app does

1. **Build blocks.** Add components (from a recipe, a technique applied
   to an ingredient, or a flavour), each with roles and how it keeps.
2. **Plan with slots.** Pick a template for a meal and fill its slots;
   the plan rolls up the week's calories, protein and cost.
3. **Prep backwards.** The plan becomes a prep session: what to cook,
   how much, in what order, and what to buy.
4. **Cook from the fridge.** "What can I make tonight": every template
   whose slots can be filled from what is prepped, ranked by what expires
   first and what you have not eaten lately; the count of possible meals
   shown, as in the video ("48 meals from this prep").
5. **Eat and log.** Taking a meal draws its amounts from the fridge and
   logs its nutrition.

## On Drive

- **Spine:** a prep session is an **Event** in the calendar; the
  shopping list's items go to the **inbox** as Tasks (or stay a list,
  linked); meals can belong to a **Project** (a cut, a party).
- **A household is an org:** shared plan, fridge and shopping list,
  edited live (two people ticking off prep steps).
- **Links:** a plan or a component shared with a guest, nothing
  installed.
- **Cards:** "Tonight: teriyaki chicken bowl, 657 kcal, 43 g protein";
  "Prep Sunday 2 pm: 4 components".
- **Media:** photos and videos of components and meals as blobs.

## First prototype (M9 of the plan)

Components with roles and keeping times; three templates (plate, rice
bowl, wrap); planning a week; the prep session and shopping list worked
out from it; "what can I make tonight" from the fridge; the prep
session's Event in the Drive app. Nutrition from a small seeded catalog;
Cooklang recipes for components.
