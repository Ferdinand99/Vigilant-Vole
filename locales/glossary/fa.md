# Persian (fa) glossary

Right-to-left. Standard Iranian Persian, neutral polite register. Word list cross-checked against Uptime Kuma's Persian
word list (terms only); differences are noted.

| English | Persian | Note |
|---|---|---|
| monitor | مانیتور | Same as Kuma; short and familiar. Plural stays singular after numerals: `{n} مانیتور` |
| monitor (verb) / monitoring | پایش | verb form «پایش کردن» used sparingly |
| heartbeat | ضربان | not used in UI; checks are «بررسی» (Kuma keeps "Heartbeat") |
| check (noun) | بررسی | `{n} بررسی` |
| check now | بررسی اکنون | short button label |
| uptime | آپتایم | Kuma: «زمان در دسترس‌بودن» (too long for tiles); «آپتایم» is widely used |
| downtime | زمان قطعی | |
| response time | زمان پاسخ | |
| incident | رویداد | status-page announcement; Kuma has none. Same word as event (context differs) |
| outage | قطعی | |
| event | رویداد | |
| maintenance / maintenance window | تعمیر و نگهداری / بازهٔ نگهداری | Kuma: «در حال تعمیر»; short tab/pill: «نگهداری» |
| status page | صفحهٔ وضعیت | same as Kuma |
| notification channel | کانال اطلاع‌رسانی | |
| notifications | اطلاع‌رسانی‌ها | same as Kuma |
| alert | هشدار | |
| escalation | ارجاع | |
| flapping | نوسان | «در حال نوسان» for the state |
| retry / retries | تلاش مجدد | same as Kuma |
| push monitor | مانیتور پوش | «پوش» transliterated |
| remote agent | عامل راه‌دور | |
| group | گروه | |
| tag | برچسب | same as Kuma |
| badge | نشان | |
| API token | توکن API | |
| two-factor authentication | احراز هویت دومرحله‌ای | same as Kuma |
| authenticator app | برنامهٔ احراز هویت | |
| recovery code | کد بازیابی | |
| backup | پشتیبان | |
| restore | بازیابی | |
| dashboard | پیشخوان | same as Kuma |
| settings | تنظیمات | |
| user | کاربر | |
| username | نام کاربری | |
| password | گذرواژه | same as Kuma |
| role | نقش | |
| admin / editor / viewer | مدیر / ویرایشگر / بیننده | |
| certificate | گواهی | «گواهی TLS» |
| expiry / expires | انقضا / منقضی می‌شود | |
| retention | نگهداری سابقه | |
| paused | متوقف | |
| resume | ازسرگیری | Kuma: «ادامه»; we use «ازسرگیری» for clarity vs. "continue" |
| pause | توقف | |
| up | برقرار | Kuma: «فعال» (ambiguous with enabled); we use «برقرار» |
| down | قطع | |
| pending | در انتظار | |
| maintenance (status) | نگهداری | |
| interval | فاصله | «هر ... بررسی» |
| timeout | مهلت | |
| agent | عامل | |
| slug | slug | kept Latin |
| target | مقصد | |
| keyword | کلیدواژه | Kuma: «کلمه کلیدی» |
| webhook | وب‌هوک | |
| Apprise | Apprise | product name |
| discovery | کشف خودکار | |
| import / export | درون‌ریزی / برون‌بری | Kuma: «ورود/استخراج اطلاعات», too long |
| configuration as code | پیکربندی به‌صورت کد | |
| session | نشست | |
| language | زبان | |
| time zone | منطقهٔ زمانی | Kuma: «موقعیت زمانی» |
| sign in / log in | ورود | |
| log out / sign out | خروج | |
| clone | تکثیر | same as Kuma |
| draft / published | پیش‌نویس / منتشرشده | |
| severity | شدت | |
| resolved | حل‌شده | |
| degraded | افت کیفیت | |
| operational | عملیاتی | |
| delete / edit / add / save / cancel | حذف / ویرایش / افزودن / ذخیره / انصراف | |
| search | جستجو | |
| host | میزبان | Docker host: «میزبان Docker» |
| token | توکن | |
| revoke | ابطال | |
| status | وضعیت | |
| metric(s) | سنجه‌ها | |

## Style

- Address: neutral polite imperative («وارد کنید», «انتخاب کنید»); no slang, no «شما» unless needed.
- Quotes « » for quoted names/values (English “ ” become « »). Punctuation ، ؛ ؟ ; keep … and the em dash.
- Letters ک ی (never ك ي). Use ZWNJ (U+200C) in می‌شود, نمی‌تواند, خانه‌ها, ‌ها plurals, etc.
- Literal digits in prose are Persian digits ۰-۹. Code-like examples (URLs, 200-299, +4712345678, OIDs, ports in code, 22:00 in examples) kept as in English or with Persian digits where purely prose.
- No sentence capitalisation in Persian. Product, protocol and technical terms stay Latin.
- Plurals: only `one` and `other`, both with a singular noun after the number.
- Dates: Gregorian calendar with Persian transliterated month names.
