<#
    scripts/collect-notices.ps1

    Builds third_party/THIRD-PARTY-NOTICES.txt from the *locked* Windows x64
    dependency graph of src-tauri/Cargo.toml.

    Why a script and not a hand-written file: the notice set has to be derived
    from an exact Cargo.lock graph, every SPDX obligation (AND / OR / WITH) has
    to be resolved deliberately, and any component whose license text is missing
    or whose license is incompatible must block publication instead of being
    silently dropped or flattened to "MIT".

    Contract:
      * input  : the release graph (cargo metadata --locked --no-default-features,
                 filtered to x86_64-pc-windows-msvc), third_party/Handy.LICENSE,
                 the vendored NSIS license text below and LICENSE.
      * output : a deterministic UTF-8 (no BOM, LF) notices file.
      * exit   : 0 = file written / check passed, 2 = publication blocked
                 (BLOCK: lines on stderr), 1 = unexpected failure.

    Scope notes (kept honest, per release plan section 4):
      * Only crates reachable through *normal* dependency edges are linked into
        speechek.exe and are listed as such.
      * Build-only crates (build-dependencies) and the packaging tools
        (tauri-cli, Bun, NSIS) are listed separately and are never presented as
        embedded in the application binary.
      * Microsoft Edge WebView2 is downloaded by the installer at install time
        and is not redistributed by Speechek.

    No network access: the fallback text for a license id comes from another
    crate in the same locked graph, so the result is reproducible from the repo
    plus the Cargo registry cache. If an id has no text anywhere in the graph,
    the collector blocks with the exact package that needed it.
#>
[CmdletBinding()]
param(
    [string]$RepoRoot = '',
    [string]$ManifestPath = 'src-tauri/Cargo.toml',
    [string]$OutputPath = 'third_party/THIRD-PARTY-NOTICES.txt',
    [string]$TargetTriple = 'x86_64-pc-windows-msvc',
    [string]$HandyLicensePath = 'third_party/Handy.LICENSE',
    # Verify the existing output is byte-identical to a fresh run instead of
    # writing it. Used by scripts/check-release.ps1.
    [switch]$Check
)

if ([string]::IsNullOrWhiteSpace($RepoRoot)) { $RepoRoot = Split-Path -Parent $PSScriptRoot }

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# SPDX resolution policy
# ---------------------------------------------------------------------------

# Licenses this project accepts and can satisfy. A branch not listed here is
# never selected for an OR; if an expression can only be satisfied by a license
# outside this set the collector blocks.
$script:AllowedIds = @(
    'MIT', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib',
    'Unicode-3.0', 'Unicode-DFS-2016', 'BSL-1.0', 'MIT-0', '0BSD', 'CC0-1.0',
    'Unlicense', 'CDLA-Permissive-2.0', 'MPL-2.0'
)

# Deterministic OR preference: when a crate offers a choice we pick the first
# entry present, so the same graph always yields the same obligation set.
$script:Priority = @(
    'MIT', 'Apache-2.0', 'BSD-2-Clause', 'BSD-3-Clause', 'ISC', 'Zlib',
    'Unicode-3.0', 'Unicode-DFS-2016', 'BSL-1.0', 'MIT-0', '0BSD', 'CC0-1.0',
    'Unlicense', 'CDLA-Permissive-2.0', 'MPL-2.0'
)

# Exception-style licenses that are permissive in practice.
$script:WithAllowed = @{
    'Apache-2.0 WITH LLVM-exception' = 'Apache-2.0'
}

# License families that cannot be satisfied by this project. Any satisfying
# operand from one of these (an AND operand, or the only operand) blocks
# publication with a concrete package name instead of being glossed over.
# NOTE: BSL-1.0 (Boost) is allowed above; "BSL" as Business Source is not.
$script:DeniedPatterns = @(
    '^GPL', '^LGPL', '^AGPL', '^SSPL', '^BUSL', '^BSL-1\.1', '^Elastic',
    '^Commons-Clause', '^CC-BY-NC', '^JSON$', '^CPL', '^CDDL', '^EUPL',
    '^OSL', '^MS-RL', '^RPL'
)


# Non-copyleft obligations worth stating explicitly in the notice file.
$script:LicenseNotes = @{
    'MPL-2.0' = 'File-level copyleft (Mozilla Public License 2.0). Speechek links the MPL-covered crates unmodified; their source is available from crates.io, so no additional source offer is required.'
    'CDLA-Permissive-2.0' = 'Community Data License Agreement - Permissive 2.0: data/files may be shared with attribution as listed above.'
    'BSL-1.0' = 'Boost Software License 1.0 (permissive). This is not the Business Source License.'
}
function Test-DeniedLicense([string]$id) {
    foreach ($p in $script:DeniedPatterns) { if ($id -match $p) { return $true } }
    return $false
}

function Get-TextHash([string]$text) {
    $norm = ($text -replace "`r`n", "`n").Trim()
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($norm)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return ([BitConverter]::ToString($sha.ComputeHash($bytes)) -replace '-', '').ToLowerInvariant() }
    finally { $sha.Dispose() }
}

# Key used to collapse license bodies that differ only by the per-crate
# "Copyright ..." header lines (those holders are listed in section 1 anyway).
function Get-LicenseBodyKey([string]$text) {
    $lines = @(($text -replace "`r`n", "`n") -split "`n" | Where-Object { $_ -notmatch '^\s*Copyright' })
    return Get-TextHash ($lines -join "`n")
}

# ---------------------------------------------------------------------------
# Host-independent ordering
# ---------------------------------------------------------------------------

# Sort-Object's -Culture resolves through the host's globalization stack: NLS
# on Windows PowerShell 5.1 (.NET Framework) and ICU on PowerShell 7 (.NET).
# The two order punctuation-bearing names differently (e.g. bit-set/bit-vec
# relative to neighbouring crates), so the committed file generated under one
# host is reported stale by -Check under the other. Ordinal comparison is
# plain code-point order, identical on every host, culture and .NET runtime.
function Sort-OrdinalStrings([string[]]$Items) {
    $arr = [string[]]@($Items)
    [System.Array]::Sort($arr, [System.StringComparer]::Ordinal)
    return $arr
}

function Sort-OrdinalObjects([object[]]$Items, [string[]]$Properties) {
    $list = New-Object System.Collections.Generic.List[object]
    foreach ($it in $Items) { $list.Add($it) }
    $props = $Properties
    $list.Sort([System.Comparison[object]]{
        param($x, $y)
        foreach ($name in $props) {
            $a = [string]$x.$name
            $b = [string]$y.$name
            if ($null -eq $a) { $a = '' }
            if ($null -eq $b) { $b = '' }
            $r = [string]::CompareOrdinal($a, $b)
            if ($r -ne 0) { return $r }
        }
        return 0
    })
    return $list.ToArray()
}

# --- SPDX expression parsing -------------------------------------------------

function Get-SpdxTokens([string]$expr) {
    # Legacy crates still use "MIT/Apache-2.0"; by SPDX convention the slash is
    # an OR. No license id contains a slash, so the rewrite is safe.
    $e = $expr -replace '/', ' OR '
    $tokens = New-Object System.Collections.Generic.List[string]
    $sb = ''
    foreach ($ch in $e.ToCharArray()) {
        if ($ch -eq '(' -or $ch -eq ')') {
            if ($sb.Trim().Length -gt 0) { [void]$tokens.Add($sb.Trim()) }
            $sb = ''
            [void]$tokens.Add([string]$ch)
        }
        elseif ([char]::IsWhiteSpace($ch)) {
            if ($sb.Trim().Length -gt 0) { [void]$tokens.Add($sb.Trim()) }
            $sb = ''
        }
        else { $sb += $ch }
    }
    if ($sb.Trim().Length -gt 0) { [void]$tokens.Add($sb.Trim()) }
    return $tokens
}

$script:toks = $null
$script:tidx = 0

function Parse-Atom {
    if ($script:tidx -ge $script:toks.Count) { throw 'unexpected end of SPDX expression' }
    $t = $script:toks[$script:tidx]
    if ($t -eq '(') {
        $script:tidx++
        $node = Parse-Expression
        if ($script:tidx -ge $script:toks.Count -or $script:toks[$script:tidx] -ne ')') {
            throw 'unbalanced "(" in SPDX expression'
        }
        $script:tidx++
        return $node
    }
    $script:tidx++
    $id = $t
    if ($script:tidx -lt $script:toks.Count -and $script:toks[$script:tidx] -eq 'WITH') {
        $script:tidx++
        if ($script:tidx -ge $script:toks.Count) { throw 'missing exception after WITH' }
        $exc = $script:toks[$script:tidx]
        $script:tidx++
        return @{ op = 'WITH'; lic = $id; exc = $exc; raw = "$id WITH $exc" }
    }
    return @{ op = 'ID'; lic = $id; raw = $id }
}

function Parse-OrList {
    $left = Parse-Atom
    while ($script:tidx -lt $script:toks.Count -and $script:toks[$script:tidx] -eq 'OR') {
        $script:tidx++
        $right = Parse-Atom
        $left = @{ op = 'OR'; kids = @($left, $right); raw = '' }
    }
    return $left
}

function Parse-Expression {
    $left = Parse-OrList
    while ($script:tidx -lt $script:toks.Count -and $script:toks[$script:tidx] -eq 'AND') {
        $script:tidx++
        $right = Parse-OrList
        $left = @{ op = 'AND'; kids = @($left, $right); raw = '' }
    }
    return $left
}

function Parse-Spdx([string]$expr) {
    $script:toks = @(Get-SpdxTokens $expr)
    $script:tidx = 0
    if ($script:toks.Count -eq 0) { throw 'empty SPDX expression' }
    $node = Parse-Expression
    if ($script:tidx -ne $script:toks.Count) { throw "trailing tokens in SPDX expression: $expr" }
    return $node
}

function Get-PriorityIndex($ids) {
    $best = [int]::MaxValue
    foreach ($id in $ids) {
        $i = [array]::IndexOf($script:Priority, $id)
        if ($i -ge 0 -and $i -lt $best) { $best = $i }
    }
    return $best
}

# Resolve one node to a set of obligations:
#   @{ ok = $bool; ids = @(...); label = '<text>'; reason = '<why it failed>' }
function Resolve-SpdxNode($node) {
    switch ($node.op) {
        'ID' {
            $id = $node.lic
            if ($script:AllowedIds -contains $id) {
                return @{ ok = $true; ids = @($id); label = $id }
            }
            if (Test-DeniedLicense $id) {
                return @{ ok = $false; ids = @(); label = $id; reason = "license '$id' is not compatible with this project" }
            }
            return @{ ok = $false; ids = @(); label = $id; reason = "license '$id' is not on the approved list" }
        }
        'WITH' {
            $key = "$($node.lic) WITH $($node.exc)"
            if ($script:WithAllowed.ContainsKey($key)) {
                return @{ ok = $true; ids = @($script:WithAllowed[$key]); label = $key }
            }
            return @{ ok = $false; ids = @(); label = $key; reason = "license exception '$key' is not approved" }
        }
        'AND' {
            $ids = New-Object System.Collections.Generic.List[string]
            $labels = New-Object System.Collections.Generic.List[string]
            foreach ($kid in $node.kids) {
                $r = Resolve-SpdxNode $kid
                if (-not $r.ok) { return $r }
                foreach ($i in $r.ids) { if (-not $ids.Contains($i)) { [void]$ids.Add($i) } }
                [void]$labels.Add($r.label)
            }
            return @{ ok = $true; ids = $ids.ToArray(); label = ($labels -join ' AND ') }
        }
        'OR' {
            $results = @()
            foreach ($kid in $node.kids) { $results += ,(Resolve-SpdxNode $kid) }
            $ok = @($results | Where-Object { $_.ok })
            if ($ok.Count -eq 0) {
                $r = $results[0]
                $labels = @($results | ForEach-Object { $_.label }) -join ' OR '
                return @{ ok = $false; ids = @(); label = $labels; reason = $r.reason }
            }
            # Pick the highest-priority acceptable branch.
            $chosen = $null
            $best = [int]::MaxValue
            foreach ($r in $ok) {
                $i = Get-PriorityIndex $r.ids
                if ($i -lt $best) { $best = $i; $chosen = $r }
            }
            $branchLabels = @($results | ForEach-Object { $_.label }) -join ' OR '
            $label = '{0} (chosen from "{1}")' -f $chosen.label, $branchLabels
            return @{ ok = $true; ids = $chosen.ids; label = $label }
        }
        default { throw "unknown SPDX node op '$($node.op)'" }
    }
}

# --- license text classification --------------------------------------------

function Get-FileLicenseIds([string]$text, [string]$fileName) {
    $ids = New-Object System.Collections.Generic.List[string]
    $base = $fileName.ToLowerInvariant()
    $names = @{
        'license-mit' = 'MIT'; 'license-mit.md' = 'MIT'; 'license-mit.txt' = 'MIT'
        'license-apache' = 'Apache-2.0'; 'license-apache.md' = 'Apache-2.0'; 'license-apache.txt' = 'Apache-2.0'
        'license-boost' = 'BSL-1.0'; 'license-zlib' = 'Zlib'; 'license-0bsd' = '0BSD'
        'license-isc' = 'ISC'; 'unlicense' = 'Unlicense'; 'license-cc0' = 'CC0-1.0'
        'license-mit-0' = 'MIT-0'; 'license-mit0' = 'MIT-0'
    }
    if ($names.ContainsKey($base)) { [void]$ids.Add($names[$base]) }

    # Content markers (checked regardless of file name, because combined
    # "LICENSE" files and name-less files are common).
    if ($text -match 'Boost Software License') { [void]$ids.Add('BSL-1.0') }
    if ($text -match 'Mozilla Public License') { [void]$ids.Add('MPL-2.0') }
    if ($text -match 'Community Data License Agreement') { [void]$ids.Add('CDLA-Permissive-2.0') }
    if ($text -match 'Apache License' -and $text -match 'Version 2\.0') { [void]$ids.Add('Apache-2.0') }
    if ($text -match 'UNICODE, INC\. LICENSE AGREEMENT') { [void]$ids.Add('Unicode-DFS-2016') }
    elseif ($text -match 'UNICODE LICENSE V3|Unicode License V3|Unicode License v3') { [void]$ids.Add('Unicode-3.0') }
    if ($text -match 'CC0 1\.0 Universal') { [void]$ids.Add('CC0-1.0') }
    if ($text -match 'free and unencumbered software released into the public domain') { [void]$ids.Add('Unlicense') }
    if ($text -match 'Redistribution and use in source and binary forms') {
        if ($text -match 'Neither the name') { [void]$ids.Add('BSD-3-Clause') }
        else { [void]$ids.Add('BSD-2-Clause') }
    }
    if ($text -match 'Permission to use, copy, modify, and(/or)? distribute this software for any purpose') {
        if ($text -match 'provided that the above copyright notice') { [void]$ids.Add('ISC') }
        else { [void]$ids.Add('0BSD') }
    }
    if ($text -match "is provided 'as-is'|is provided .as-is.|is provided \x22as-is\x22") { [void]$ids.Add('Zlib') }
    if ($text -match 'Permission is hereby granted, free of charge') {
        if ($text -match 'without restriction' -and $text -notmatch 'The above copyright notice and this permission notice shall be included') {
            [void]$ids.Add('MIT-0')
        }
        else { [void]$ids.Add('MIT') }
    }
    return $ids.ToArray()
}

function Test-LicenseFileName([string]$fileName) {
    return $fileName -match '(?i)^(license|licence|copying|notice|unlicense|copyright)'
}

function Get-LicenseFiles([string]$dir, [string]$licenseFile) {
    $files = @()
    if ($dir -and (Test-Path -LiteralPath $dir -PathType Container)) {
        $all = Get-ChildItem -LiteralPath $dir -File -ErrorAction SilentlyContinue |
            Where-Object { Test-LicenseFileName $_.Name }
        foreach ($f in (Sort-OrdinalObjects @($all) @('Name'))) { $files += $f.FullName }
    }
    if ($licenseFile -and (Test-Path -LiteralPath $licenseFile -PathType Leaf)) {
        $files += (Resolve-Path -LiteralPath $licenseFile).Path
    }
    return $files
}

# ---------------------------------------------------------------------------
# Vendored non-crate components
# ---------------------------------------------------------------------------

# Verbatim NSIS COPYING. Source:
#   https://github.com/kichik/nsis/blob/master/COPYING (retrieved 2026-10-02)
# The NSIS installer stub is embedded in the generated setup executable.
$script:NsisLicense = @'
NSIS
====

COPYRIGHT
---------

Copyright (C) 1999-2026 Contributors

More detailed copyright information can be found in the individual source code files.

APPLICABLE LICENSES
-------------------

* All NSIS source code, plug-ins, documentation, examples, header files and graphics, with the exception of the compression modules and where otherwise noted, are licensed under the zlib/libpng license.

* The zlib compression module for NSIS is licensed under the zlib/libpng license.

* The bzip2 compression module for NSIS is licensed under the bzip2 license.

* The LZMA compression module for NSIS is licensed under the Common Public License version 1.0.

ZLIB/LIBPNG LICENSE
-------------------

This software is provided 'as-is', without any express or implied warranty. In no event will the authors be held liable for any damages arising from the use of this software.

Permission is granted to anyone to use this software for any purpose, including commercial applications, and to alter it and redistribute it freely, subject to the following restrictions:

      1. The origin of this software must not be misrepresented; you must not claim that you wrote the original software. If you use this software in a product, an acknowledgment in the product documentation would be appreciated but is not required.

      2. Altered source versions must be plainly marked as such, and must not be misrepresented as being the original software.

      3. This notice may not be removed or altered from any source distribution.

BZIP2 LICENSE
-------------

This program, "bzip2" and associated library "libbzip2", are copyright (C) 1996-2000 Julian R Seward. All rights reserved.

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

      1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.

      2. The origin of this software must not be misrepresented; you must not claim that you wrote the original software. If you use this software in a product, an acknowledgment in the product documentation would be appreciated but is not required.

      3. Altered source versions must be plainly marked as such, and must not be misrepresented as being the original software.

      4. The name of the author may not be used to endorse or promote products derived from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Julian Seward, Cambridge, UK.

jseward@acm.org

COMMON PUBLIC LICENSE VERSION 1.0
---------------------------------

THE ACCOMPANYING PROGRAM IS PROVIDED UNDER THE TERMS OF THIS COMMON PUBLIC LICENSE ("AGREEMENT"). ANY USE, REPRODUCTION OR DISTRIBUTION OF THE PROGRAM CONSTITUTES RECIPIENT'S ACCEPTANCE OF THIS AGREEMENT.

1. DEFINITIONS

"Contribution" means:

      a) in the case of the initial Contributor, the initial code and documentation distributed under this Agreement, and
      b) in the case of each subsequent Contributor:

      i) changes to the Program, and

      ii) additions to the Program;

      where such changes and/or additions to the Program originate from and are distributed by that particular Contributor. A Contribution 'originates' from a Contributor if it was added to the Program by such Contributor itself or anyone acting on such Contributor's behalf. Contributions do not include additions to the Program which: (i) are separate modules of software distributed in conjunction with the Program under their own license agreement, and (ii) are not derivative works of the Program.

"Contributor" means any person or entity that distributes the Program.

"Licensed Patents " mean patent claims licensable by a Contributor which are necessarily infringed by the use or sale of its Contribution alone or when combined with the Program.

"Program" means the Contributions distributed in accordance with this Agreement.

"Recipient" means anyone who receives the Program under this Agreement, including all Contributors.

2. GRANT OF RIGHTS

      a) Subject to the terms of this Agreement, each Contributor hereby grants Recipient a non-exclusive, worldwide, royalty-free copyright license to reproduce, prepare derivative works of, publicly display, publicly perform, distribute and sublicense the Contribution of such Contributor, if any, and such derivative works, in source code and object code form.

      b) Subject to the terms of this Agreement, each Contributor hereby grants Recipient a non-exclusive, worldwide, royalty-free patent license under Licensed Patents to make, use, sell, offer to sell, import and otherwise transfer the Contribution of such Contributor, if any, in source code and object code form. This patent license shall apply to the combination of the Contribution and the Program if, at the time the Contribution is added by the Contributor, such addition of the Contribution causes such combination to be covered by the Licensed Patents. The patent license shall not apply to any other combinations which include the Contribution. No hardware per se is licensed hereunder.

      c) Recipient understands that although each Contributor grants the licenses to its Contributions set forth herein, no assurances are provided by any Contributor that the Program does not infringe the patent or other intellectual property rights of any other entity. Each Contributor disclaims any liability to Recipient for claims brought by any other entity based on infringement of intellectual property rights or otherwise. As a condition to exercising the rights and licenses granted hereunder, each Recipient hereby assumes sole responsibility to secure any other intellectual property rights needed, if any. For example, if a third party patent license is required to allow Recipient to distribute the Program, it is Recipient's responsibility to acquire that license before distributing the Program.

      d) Each Contributor represents that to its knowledge it has sufficient copyright rights in its Contribution, if any, to grant the copyright license set forth in this Agreement.

3. REQUIREMENTS

A Contributor may choose to distribute the Program in object code form under its own license agreement, provided that:

      a) it complies with the terms and conditions of this Agreement; and

      b) its license agreement:

      i) effectively disclaims on behalf of all Contributors all warranties and conditions, express and implied, including warranties or conditions of title and non-infringement, and implied warranties or conditions of merchantability and fitness for a particular purpose;

      ii) effectively excludes on behalf of all Contributors all liability for damages, including direct, indirect, special, incidental and consequential damages, such as lost profits;

      iii) states that any provisions which differ from this Agreement are offered by that Contributor alone and not by any other party; and

      iv) states that source code for the Program is available from such Contributor, and informs licensees how to obtain it in a reasonable manner on or through a medium customarily used for software exchange.

When the Program is made available in source code form:

      a) it must be made available under this Agreement; and

      b) a copy of this Agreement must be included with each copy of the Program.

Contributors may not remove or alter any copyright notices contained within the Program.

Each Contributor must identify itself as the originator of its Contribution, if any, in a manner that reasonably allows subsequent Recipients to identify the originator of the Contribution.

4. COMMERCIAL DISTRIBUTION

Commercial distributors of software may accept certain responsibilities with respect to end users, business partners and the like. While this license is intended to facilitate the commercial use of the Program, the Contributor who includes the Program in a commercial product offering should do so in a manner which does not create potential liability for other Contributors. Therefore, if a Contributor includes the Program in a commercial product offering, such Contributor ("Commercial Contributor") hereby agrees to defend and indemnify every other Contributor ("Indemnified Contributor") against any losses, damages and costs (collectively "Losses") arising from claims, lawsuits and other legal actions brought by a third party against the Indemnified Contributor to the extent caused by the acts or omissions of such Commercial Contributor in connection with its distribution of the Program in a commercial product offering. The obligations in this section do not apply to any claims or Losses relating to any actual or alleged intellectual property infringement. In order to qualify, an Indemnified Contributor must: a) promptly notify the Commercial Contributor in writing of such claim, and b) allow the Commercial Contributor to control, and cooperate with the Commercial Contributor in, the defense and any related settlement negotiations. The Indemnified Contributor may participate in any such claim at its own expense.

For example, a Contributor might include the Program in a commercial product offering, Product X. That Contributor is then a Commercial Contributor. If that Commercial Contributor then makes performance claims, or offers warranties related to Product X, those performance claims and warranties are such Commercial Contributor's responsibility alone. Under this section, the Commercial Contributor would have to defend claims against the other Contributors related to those performance claims and warranties, and if a court requires any other Contributor to pay any damages as a result, the Commercial Contributor must pay those damages.

5. NO WARRANTY

EXCEPT AS EXPRESSLY SET FORTH IN THIS AGREEMENT, THE PROGRAM IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, EITHER EXPRESS OR IMPLIED INCLUDING, WITHOUT LIMITATION, ANY WARRANTIES OR CONDITIONS OF TITLE, NON-INFRINGEMENT, MERCHANTABILITY OR FITNESS FOR A PARTICULAR PURPOSE. Each Recipient is solely responsible for determining the appropriateness of using and distributing the Program and assumes all risks associated with its exercise of rights under this Agreement, including but not limited to the risks and costs of program errors, compliance with applicable laws, damage to or loss of data, programs or equipment, and unavailability or interruption of operations.

6. DISCLAIMER OF LIABILITY

EXCEPT AS EXPRESSLY SET FORTH IN THIS AGREEMENT, NEITHER RECIPIENT NOR ANY CONTRIBUTORS SHALL HAVE ANY LIABILITY FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING WITHOUT LIMITATION LOST PROFITS), HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OR DISTRIBUTION OF THE PROGRAM OR THE EXERCISE OF ANY RIGHTS GRANTED HEREUNDER, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGES.

7. GENERAL

If any provision of this Agreement is invalid or unenforceable under applicable law, it shall not affect the validity or enforceability of the remainder of the terms of this Agreement, and without further action by the parties hereto, such provision shall be reformed to the minimum extent necessary to make such provision valid and enforceable.

If Recipient institutes patent litigation against a Contributor with respect to a patent applicable to software (including a cross-claim or counterclaim in a lawsuit), then any patent licenses granted by that Contributor to such Recipient under this Agreement shall terminate as of the date such litigation is filed. In addition, if Recipient institutes patent litigation against any entity (including a cross-claim or counterclaim in a lawsuit) alleging that the Program itself (excluding combinations of the Program with other software or hardware) infringes such Recipient's patent(s), then such Recipient's rights granted under Section 2(b) shall terminate as of the date such litigation is filed.

All Recipient's rights under this Agreement shall terminate if it fails to comply with any of the material terms or conditions of this Agreement and does not cure such failure in a reasonable period of time after becoming aware of such noncompliance. If all Recipient's rights under this Agreement terminate, Recipient agrees to cease use and distribution of the Program as soon as reasonably practicable. However, Recipient's obligations under this Agreement and any licenses granted by Recipient relating to the Program shall continue and survive.

Everyone is permitted to copy and distribute copies of this Agreement, but in order to avoid inconsistency the Agreement is copyrighted and may only be modified in the following manner. The Agreement Steward reserves the right to publish new versions (including revisions) of this Agreement from time to time. No one other than the Agreement Steward has the right to modify this Agreement. IBM is the initial Agreement Steward. IBM may assign the responsibility to serve as the Agreement Steward to a suitable separate entity. Each new version of the Agreement will be given a distinguishing version number. The Program (including Contributions) may always be distributed subject to the version of the Agreement under which it was received. In addition, after a new version of the Agreement is published, Contributor may elect to distribute the Program (including its Contributions) under the new version. Except as expressly stated in Sections 2(a) and 2(b) above, Recipient receives no rights or licenses to the intellectual property of any Contributor under this Agreement, whether expressly, by implication, estoppel or otherwise. All rights in the Program not expressly granted under this Agreement are reserved.

This Agreement is governed by the laws of the State of New York and the intellectual property laws of the United States of America. No party to this Agreement will bring a legal action under this Agreement more than one year after the cause of action arose. Each party waives its rights to a jury trial in any resulting litigation.

SPECIAL EXCEPTION FOR LZMA COMPRESSION MODULE
---------------------------------------------

Igor Pavlov and Amir Szekely, the authors of the LZMA compression module for NSIS, expressly permit you to statically or dynamically link your code (or bind by name) to the files from the LZMA compression module for NSIS without subjecting your linked code to the terms of the Common Public license version 1.0. Any modifications or additions to files from the LZMA compression module for NSIS, however, are subject to the terms of the Common Public License version 1.0.
'@

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

$manifestFull = Join-Path $RepoRoot $ManifestPath
if (-not (Test-Path -LiteralPath $manifestFull -PathType Leaf)) {
    throw "manifest not found: $manifestFull"
}
function Sort-Packages($ids, $pkgById) {
    $objs = foreach ($i in $ids) {
        $p = $pkgById[$i]
        [pscustomobject]@{ Id = $i; Name = $p.name; Version = $p.version; Pkg = $p }
    }
    return @(Sort-OrdinalObjects $objs @('Name', 'Version'))
}

function Get-Closure($rootId, $kind, $nodeById) {
    $seen = @{}
    $stack = New-Object System.Collections.Stack
    $stack.Push($rootId)
    while ($stack.Count -gt 0) {
        $id = $stack.Pop()
        if ($seen.ContainsKey($id)) { continue }
        $seen[$id] = $true
        $node = $nodeById[$id]
        if (-not $node) { continue }
        foreach ($dep in $node.deps) {
            $hit = $false
            foreach ($dk in $dep.dep_kinds) {
                $k = $dk.kind
                if ($kind -eq 'normal' -and ($null -eq $k -or $k -eq 'normal')) { $hit = $true }
                elseif ($kind -eq 'build' -and $k -eq 'build') { $hit = $true }
                elseif ($kind -eq 'dev' -and $k -eq 'dev') { $hit = $true }
            }
            if ($hit) { $stack.Push($dep.pkg) }
        }
    }
    $seen.Remove($rootId)
    return @($seen.Keys)
}

Push-Location $RepoRoot
try {
    $raw = & cargo metadata --format-version 1 --locked --manifest-path $manifestFull `
        --no-default-features --filter-platform $TargetTriple
    if ($LASTEXITCODE -ne 0) { throw "cargo metadata failed with exit code $LASTEXITCODE" }
}
finally { Pop-Location }

$meta = ($raw | Out-String) | ConvertFrom-Json
$pkgById = @{}
foreach ($p in $meta.packages) { $pkgById[$p.id] = $p }
$nodeById = @{}
foreach ($n in $meta.resolve.nodes) { $nodeById[$n.id] = $n }

$rootPkg = $meta.packages | Where-Object { $_.name -eq 'speechek' } | Select-Object -First 1
if (-not $rootPkg) { throw 'workspace package "speechek" not found in cargo metadata' }

$runtimeSorted = Sort-Packages (Get-Closure $rootPkg.id 'normal' $nodeById) $pkgById
$runtimeIdSet = @{}
foreach ($e in $runtimeSorted) { $runtimeIdSet[$e.Id] = $true }
$buildSorted = Sort-Packages (@(Get-Closure $rootPkg.id 'build' $nodeById | Where-Object { -not $runtimeIdSet.ContainsKey($_) })) $pkgById

$blockers = New-Object System.Collections.Generic.List[string]

# --- pool of real license texts (fallback donor for ids) --------------------

$pool = @{}
foreach ($entry in $runtimeSorted) {
    $dir = Split-Path -Parent $entry.Pkg.manifest_path
    foreach ($file in @(Get-LicenseFiles $dir $entry.Pkg.license_file)) {
        $text = [System.IO.File]::ReadAllText($file)
        foreach ($id in (Get-FileLicenseIds $text ([System.IO.Path]::GetFileName($file)))) {
            if (-not $pool.ContainsKey($id)) {
                $pool[$id] = @{ Text = $text; Source = "$($entry.Name) $($entry.Version) ($([System.IO.Path]::GetFileName($file)))" }
            }
        }
    }
}

# --- resolve runtime packages ----------------------------------------------

$components = New-Object System.Collections.Generic.List[object]
$licenseTexts = @{}   # hash -> @{ Text; Ids = List; UsedBy = List }
$extraTexts = @{}     # hash -> @{ Text; Who; Name }
$extraCandidates = New-Object System.Collections.Generic.List[object]

function Add-LicenseText($hash, $text, $id, $who) {
    if (-not $licenseTexts.ContainsKey($hash)) {
        $licenseTexts[$hash] = @{ Text = $text; Ids = (New-Object System.Collections.Generic.List[string]); UsedBy = (New-Object System.Collections.Generic.List[string]) }
    }
    if ($id -and -not $licenseTexts[$hash].Ids.Contains($id)) { [void]$licenseTexts[$hash].Ids.Add($id) }
    if (-not $licenseTexts[$hash].UsedBy.Contains($who)) { [void]$licenseTexts[$hash].UsedBy.Add($who) }
}

foreach ($entry in $runtimeSorted) {
    $p = $entry.Pkg
    $who = "$($entry.Name) $($entry.Version)"
    $dir = Split-Path -Parent $p.manifest_path
    $fileInfo = @()
    foreach ($file in @(Get-LicenseFiles $dir $p.license_file)) {
        $text = [System.IO.File]::ReadAllText($file)
        $fileInfo += ,@{ Path = $file; Name = [System.IO.Path]::GetFileName($file); Text = $text; Ids = @(Get-FileLicenseIds $text ([System.IO.Path]::GetFileName($file))) }
    }

    if ([string]::IsNullOrWhiteSpace($p.license)) {
        $blockers.Add("$who declares no license expression and ships no license file")
        continue
    }

    try { $node = Parse-Spdx $p.license }
    catch { $blockers.Add("$who has an unparseable license expression '$($p.license)': $_"); continue }

    $res = Resolve-SpdxNode $node
    if (-not $res.ok) { $blockers.Add("$who ($($p.license)): $($res.reason)"); continue }

    $missing = $false
    foreach ($id in $res.ids) {
        $own = @($fileInfo | Where-Object { $_.Ids -contains $id } | Select-Object -First 1)
        if ($own.Count -gt 0) {
            Add-LicenseText (Get-LicenseBodyKey $own[0].Text) $own[0].Text $id $who
        }
        elseif ($pool.ContainsKey($id)) {
            Add-LicenseText (Get-LicenseBodyKey $pool[$id].Text) $pool[$id].Text $id $who
        }
        else {
            $blockers.Add("$who needs the text of '$id' but no license text for that id exists in the locked graph")
            $missing = $true
        }
    }
    if ($missing) { continue }

    # Candidate extra notices: files whose license ids are not all already
    # covered above (unclassified NOTICE/third-party files, or a license branch
    # the crate offers but we do not rely on). Filtered after the loop so that
    # a text already present in section 2 is not repeated.
    foreach ($fi in $fileInfo) {
        $covered = @($fi.Ids | Where-Object { $res.ids -contains $_ })
        if ($fi.Ids.Count -eq 0 -or $covered.Count -lt $fi.Ids.Count) {
            [void]$extraCandidates.Add(@{ Key = (Get-LicenseBodyKey $fi.Text); Text = $fi.Text; Who = $who; Name = $fi.Name })
        }
    }

    $copyright = @()
    foreach ($fi in $fileInfo) {
        foreach ($line in ($fi.Text -split "`n")) {
            if ($line -match '^\s*Copyright') { $copyright += $line.Trim() }
        }
    }

    $components.Add([pscustomobject]@{
        Name = $entry.Name; Version = $entry.Version; License = $p.license
        Satisfied = ($res.ids -join ' AND '); Label = $res.label
        Repository = $p.repository; Copyright = @($copyright | Select-Object -Unique)
    })
}

# Handy is vendored (paste transaction + overlay geometry), not a crate.
$handyFull = Join-Path $RepoRoot $HandyLicensePath
if (-not (Test-Path -LiteralPath $handyFull -PathType Leaf)) {
    $blockers.Add("vendored component license missing: $HandyLicensePath")
}
else {
    $handyText = [System.IO.File]::ReadAllText($handyFull)
    Add-LicenseText (Get-TextHash $handyText) $handyText 'MIT' 'Handy (vendored)'
}
Add-LicenseText (Get-TextHash $script:NsisLicense) $script:NsisLicense 'Zlib' 'NSIS installer engine (vendored)'

foreach ($c in $extraCandidates) {
    if ($licenseTexts.ContainsKey($c.Key)) { continue }
    if ($extraTexts.ContainsKey($c.Key)) { continue }
    $extraTexts[$c.Key] = @{ Text = $c.Text; Who = $c.Who; Name = $c.Name }
}

if ($blockers.Count -gt 0) {
    foreach ($b in $blockers) { [Console]::Error.WriteLine("BLOCK: $b") }
    [Console]::Error.WriteLine("Publication blocked: $($blockers.Count) license problem(s).")
    exit 2
}

# ---------------------------------------------------------------------------
# Render
# ---------------------------------------------------------------------------

$LF = "`n"
$sb = New-Object System.Text.StringBuilder

function Add-Line([string]$s) { [void]$sb.Append($s); [void]$sb.Append($LF) }

Add-Line 'Speechek - Third-Party Notices'
Add-Line '================================'
Add-Line ''
Add-Line 'Generated by scripts/collect-notices.ps1 from the locked Windows x64'
Add-Line 'dependency graph of src-tauri/Cargo.toml'
Add-Line "(cargo metadata --locked --no-default-features --filter-platform $TargetTriple)."
Add-Line ''
Add-Line 'Speechek itself is licensed under the MIT License; see the LICENSE file'
Add-Line 'next to the installed executable.'
Add-Line ''
Add-Line 'This file is embedded in speechek.exe and also shipped beside the'
Add-Line 'installed executable, so the distributed application stands alone.'
Add-Line ''
Add-Line 'Sections:'
Add-Line '  1. Components linked into speechek.exe'
Add-Line '  2. License texts'
Add-Line '  3. Additional notices shipped by crates'
Add-Line '  4. Build-time only dependencies (not distributed in speechek.exe)'
Add-Line '  5. Packaging tools and vendored components (not Rust crates)'
Add-Line '  6. Runtime prerequisites not redistributed by Speechek'
Add-Line ''

Add-Line ('-' * 76)
Add-Line '1. Components linked into speechek.exe'
Add-Line ('-' * 76)
Add-Line ''
foreach ($c in $components) {
    Add-Line ("{0} {1}" -f $c.Name, $c.Version)
    Add-Line ("    Declared license : {0}" -f $c.License)
    Add-Line ("    Satisfied by     : {0}" -f $c.Label)
    if ($c.Repository) { Add-Line ("    Repository       : {0}" -f $c.Repository) }
    foreach ($cp in $c.Copyright) { Add-Line ("    Copyright        : {0}" -f $cp) }
    Add-Line ''
}

Add-Line ('-' * 76)
Add-Line '2. License texts'
Add-Line ('-' * 76)
Add-Line ''
$textKeys = Sort-OrdinalStrings @($licenseTexts.Keys)
$ordered = foreach ($k in $textKeys) {
    $t = $licenseTexts[$k]
    $ids = @(Sort-OrdinalStrings @($t.Ids))
    [pscustomobject]@{ Key = $k; Ids = $ids; UsedBy = @(Sort-OrdinalStrings @($t.UsedBy)); Text = $t.Text; SortKey = ($ids -join ',') }
}
# List<T>.Sort is not stable, so equal SortKey values (the same id set with
# different text bodies) must be ordered by the content hash too, or the two
# .NET runtimes would order them differently.
$ordered = @(Sort-OrdinalObjects @($ordered) @('SortKey', 'Key'))
foreach ($t in $ordered) {
    Add-Line ('=' * 76)
    Add-Line ("License: {0}" -f ($t.Ids -join ', '))
    Add-Line ('=' * 76)
    foreach ($nid in $t.Ids) {
        if ($script:LicenseNotes.ContainsKey($nid)) { Add-Line ("Note: {0}" -f $script:LicenseNotes[$nid]) }
    }
    Add-Line ''
    Add-Line ($t.Text.TrimEnd())
    Add-Line ''
    Add-Line 'Used by:'
    foreach ($u in $t.UsedBy) { Add-Line ("  - {0}" -f $u) }
    Add-Line ''
}

Add-Line ('-' * 76)
Add-Line '3. Additional notices shipped by crates'
Add-Line ('-' * 76)
Add-Line ''
$extraKeys = Sort-OrdinalStrings @($extraTexts.Keys)
$extraOrdered = foreach ($k in $extraKeys) {
    $t = $extraTexts[$k]
    [pscustomobject]@{ Key = $k; Who = $t.Who; Name = $t.Name; Text = $t.Text }
}
$extraOrdered = @(Sort-OrdinalObjects @($extraOrdered) @('Who', 'Name', 'Key'))
if ($extraOrdered.Count -eq 0) { Add-Line '(none)' ; Add-Line '' }
foreach ($t in $extraOrdered) {
    Add-Line ("--- {0} - {1} ---" -f $t.Who, $t.Name)
    Add-Line ''
    Add-Line ($t.Text.TrimEnd())
    Add-Line ''
}

Add-Line ('-' * 76)
Add-Line '4. Build-time only dependencies (not distributed in speechek.exe)'
Add-Line ('-' * 76)
Add-Line ''
foreach ($e in $buildSorted) {
    Add-Line ("{0} {1} - {2}" -f $e.Name, $e.Version, $e.Pkg.license)
}
Add-Line ''

Add-Line ('-' * 76)
Add-Line '5. Packaging tools and vendored components (not Rust crates)'
Add-Line ('-' * 76)
Add-Line ''
Add-Line 'tauri-cli 2.12.0 - MIT OR Apache-2.0'
Add-Line '    Bundles the application and the NSIS installer.'
Add-Line '    Its NSIS template is forked as src-tauri/nsis/installer.nsi; upstream'
Add-Line '    attribution and license reference are retained in that file header.'
Add-Line ''
Add-Line 'Handy (vendored, not a Rust crate) - MIT'
Add-Line '    Copyright (c) 2025 CJ Pais.'
Add-Line '    The receipt-sequenced paste transaction and the overlay geometry are'
Add-Line '    adapted from Handy. The license text is bundled with the application'
Add-Line '    (third_party/Handy.LICENSE, also served at /licenses/Handy.LICENSE).'
Add-Line ''
Add-Line 'NSIS (Nullsoft Scriptable Install System) - zlib/libpng, bzip2, CPL-1.0'
Add-Line '    The generated setup executable embeds the NSIS installer stub.'
Add-Line '    Copyright (C) 1999-2026 Contributors.'
Add-Line '    Full license text follows.'
Add-Line ''
Add-Line ($script:NsisLicense.TrimEnd())
Add-Line ''
Add-Line 'Bun 1.4.2 - MIT'
Add-Line '    Runs the frontend build step; not shipped with Speechek.'
Add-Line ''

Add-Line ('-' * 76)
Add-Line '6. Runtime prerequisites not redistributed by Speechek'
Add-Line ('-' * 76)
Add-Line ''
Add-Line 'Microsoft Edge WebView2 Runtime is required to display the application'
Add-Line 'windows. The installer downloads WebView2 from Microsoft at install time'
Add-Line '(downloadBootstrapper) and Speechek does not redistribute or include it.'
Add-Line ''

$output = $sb.ToString()

# ---------------------------------------------------------------------------
# Write / check
# ---------------------------------------------------------------------------

$outputFull = Join-Path $RepoRoot $OutputPath
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
if ($Check) {
    if (-not (Test-Path -LiteralPath $outputFull -PathType Leaf)) {
        [Console]::Error.WriteLine("BLOCK: $OutputPath does not exist; run scripts/collect-notices.ps1")
        exit 2
    }
    $existing = [System.IO.File]::ReadAllText($outputFull)
    if ($existing -ne $output) {
        [Console]::Error.WriteLine("BLOCK: $OutputPath is stale; run scripts/collect-notices.ps1")
        exit 2
    }
    Write-Host "notices up to date: $OutputPath"
    exit 0
}

$dirOut = Split-Path -Parent $outputFull
if ($dirOut -and -not (Test-Path -LiteralPath $dirOut)) { New-Item -ItemType Directory -Path $dirOut | Out-Null }
[System.IO.File]::WriteAllText($outputFull, $output, $utf8NoBom)
Write-Host "wrote $OutputPath ($($output.Length) chars, $($components.Count) crates, $($ordered.Count) license texts)"
