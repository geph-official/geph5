# Terms of service

By using or paying for Gephyra OÜ's Geph service, you agree to these terms. Any material changes to these terms will be notified via a prominent notice on https://geph.io at least one month before the changes are applied. If you wish to exercise your right to reject such changes, you should stop using the service.

## The service

Geph uses a custom open-source architecture to tunnel traffic through a variety of recognition-resistant protocols in order to protect users' freedom on the internet. It also protects personal information with the use of encryption and masks user metadata by hiding the user's IP address and replacing it with one of ours.

To protect ourselves, our customers, and the quality of our service, we block the following TCP ports:

- Port 25 to prevent email spam

We do not block or filter domains except when requested by the owner of an IP address or when known botnet addresses cause our servers to be null-routed by hosting providers.

We do not modify, redirect, or inject data into users' traffic.

## Forbidden activities

- Unauthorized reselling of Geph services
- Email and other spam
- Any activities illegal in the jurisdiction of the selected exit server
- Automated registration of accounts

## Applicable laws

The terms shall be construed in accordance with and governed by the substantive laws of the Republic of Estonia.

## Customer support

We offer official customer support only via email to support@geph.io

---

# Non-cooperation policy

We will never disclose to any third party any non-public information on our users, unless legally compelled to under the laws of the Republic of Estonia. Any such legal requests will be documented in as much detail as legally possible.

**As of Mar 1 2025, no such requests have ever been received.**

We will not in any circumstance allow third parties direct "backdoor" access to our servers. We commit to moving to a different jurisdiction if compulsory backdoor access ever becomes possible in the Republic of Estonia.

---

# No-logging policy

We do not keep user activity details of any kind, and maintain the minimum amount of data required to authenticate users and process payments.

## What do we store

The only two types of user data we store persistently are authentication credentials and payment processing data.

### Authentication credentials

For every user, we store a username, a `Argon2` hardened password hash, the time at which the user was created, and the time at which the user last logged in. For example:

```
 id | username | pwdhash |        createtime          |        last_login
----+----------+---------+----------------------------+----------------------------
 51 | pwtest   | $arg... | 2019-12-29 12:34:28.72295  |  2022-12-23 17:58:30.9346
```

### Payment processing

We use Stripe for card payments, ChangeNOW for cryptocurrency payments, and various 3rd-party providers for Alipay and Wechat payments. The following is what we store on our systems; please see the privacy policies of Stripe and ChangeNOW for how they deal with your data.

#### Transaction history

We store a list of all payment activity per user:

```
 invoice_id | days | amount |      created_at     |        paid_at      | user_id | pay_method | metadata
------------+------+--------+---------------------+---------------------+---------+------------+-------------
         74 |   30 |    500 | 2022-07-17 12:07:18 | 2022-07-17 12:10:18 |     3   |    stripe  |    Plus
```

## What we **NEVER** store

We never store for more than 24 hours any of the following information:

- User traffic

- DNS requests

- Detailed statistics

- IP addresses

We _do_ gather and store aggregate statistics, which are publicly viewable at https://is.gd/gephdash2
