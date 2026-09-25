## Default Permission

Nostos Tauri plugin — default permissions for the connect/subscribe/write/query/checkpoint/watch/set-token/sign-out + push-token register/deregister surface. A shipped plugin would add scoped per-table permission sets; this scaffold grants all ten commands unconditionally.

#### This default permission set includes the following:

- `allow-connect`
- `allow-subscribe`
- `allow-write`
- `allow-query`
- `allow-checkpoint`
- `allow-watch`
- `allow-set-token`
- `allow-sign-out`
- `allow-register-push-token`
- `allow-deregister-push-token`
- `allow-or-set-add`
- `allow-or-set-remove`
- `allow-counter-increment`
- `allow-counter-decrement`
- `allow-dead-letters`
- `allow-connection-state`

## Permission Table

<table>
<tr>
<th>Identifier</th>
<th>Description</th>
</tr>


<tr>
<td>

`nostos:allow-checkpoint`

</td>
<td>

Enables the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-checkpoint`

</td>
<td>

Denies the checkpoint command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-connect`

</td>
<td>

Enables the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-connect`

</td>
<td>

Denies the connect command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-connection-state`

</td>
<td>

Enables the connection_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-connection-state`

</td>
<td>

Denies the connection_state command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-counter-decrement`

</td>
<td>

Enables the counter_decrement command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-counter-decrement`

</td>
<td>

Denies the counter_decrement command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-counter-increment`

</td>
<td>

Enables the counter_increment command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-counter-increment`

</td>
<td>

Denies the counter_increment command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-dead-letters`

</td>
<td>

Enables the dead_letters command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-dead-letters`

</td>
<td>

Denies the dead_letters command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-deregister-push-token`

</td>
<td>

Enables the deregister_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-deregister-push-token`

</td>
<td>

Denies the deregister_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-or-set-add`

</td>
<td>

Enables the or_set_add command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-or-set-add`

</td>
<td>

Denies the or_set_add command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-or-set-remove`

</td>
<td>

Enables the or_set_remove command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-or-set-remove`

</td>
<td>

Denies the or_set_remove command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-query`

</td>
<td>

Enables the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-query`

</td>
<td>

Denies the query command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-register-push-token`

</td>
<td>

Enables the register_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-register-push-token`

</td>
<td>

Denies the register_push_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-set-token`

</td>
<td>

Enables the set_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-set-token`

</td>
<td>

Denies the set_token command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-sign-out`

</td>
<td>

Enables the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-sign-out`

</td>
<td>

Denies the sign_out command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-subscribe`

</td>
<td>

Enables the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-subscribe`

</td>
<td>

Denies the subscribe command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-watch`

</td>
<td>

Enables the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-watch`

</td>
<td>

Denies the watch command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:allow-write`

</td>
<td>

Enables the write command without any pre-configured scope.

</td>
</tr>

<tr>
<td>

`nostos:deny-write`

</td>
<td>

Denies the write command without any pre-configured scope.

</td>
</tr>
</table>
